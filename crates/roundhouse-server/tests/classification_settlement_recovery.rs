// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Recovering a classification charge whose settlement was never confirmed.
//!
//! `classification_runtime.rs` proves the negative this suite exists to end: a
//! settle that failed leaves a durable record saying so and nothing ever asks
//! the ledger again. What that costs is one call's accounting — the service
//! billed, and whether this deployment's ledger moved is a question nobody
//! asks again, so a reader who trusts the ledger cannot tell a charge that
//! never landed from an acknowledgement that was lost after it did.
//!
//! **Three states, and the whole suite is about keeping them apart.** What the
//! service reported it billed (`EvaluationUsage`), what this deployment's rate
//! card prices that at (`EvaluationSpend::Measured::usd`), and what the
//! evaluation ledger has confirmed it applied (`SettlementAck`). A recovery
//! that collapsed any two of them would either charge twice or book a billed
//! call as free.
//!
//! Every claim here is asserted against **committed dollars on the ledger**
//! rather than against a count of settle calls, because recovery is two turns
//! long: the charge lands when the background repair's `settle_grant` returns,
//! and the durable acknowledgement lands on the next turn's writer. Between
//! those, a re-driven settle is correct and deduplicates, so call counts move
//! while the committed total must not. Counts are asserted only for the claims
//! that are about counts — one HTTP purchase, and no second grant.
//!
//! The durable repair record is asserted through its *serialized* tag rather
//! than through a Rust variant, for the same reason the accounting is asserted
//! through the ledger: what a successor process reads back is the bytes.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassifierIdentity,
    ContextDependence, EvaluationSpend, EvaluationUsage, Graded, ReservationRecord, SettlementAck,
    TAXONOMY_VERSION, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Balance, BalanceQuery, BudgetTerms, BudgetWindow, Grant, GrantRequest, MemorySpendLedger,
    Principal, Settled, Settlement, SettlementKey, SpendError, SpendLedger, TargetFilter,
    TurnPolicy,
};
use roundhouse_core::event::{CacheReadSource, SessionEvent, SessionEventKind};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, ProviderPricing, Target};
use roundhouse_core::store::{Lease, MemoryStore, SessionStore, StoreError};
use roundhouse_fleet::LocalFleet;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierModelSpec,
    FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor};

mod common;

/// The serialized `type` tag of the durable repair acknowledgement.
///
/// A literal rather than a Rust variant: this suite's subject is what a
/// successor process reads back out of the log, and a test that asserted on the
/// in-process enum would keep passing if the record stopped being written in a
/// shape anything else could read.
const REPAIR_TAG: &str = "classification_settlement_repaired";

// ---------------------------------------------------------------- classifier

/// An answer carrying usage, so the spend is `Measured` and has a price.
const ANSWER: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// The same answer with **no usage block at all**.
///
/// The service answered and said nothing about what it billed. That is
/// `EvaluationSpend::Unknown`, and the one thing recovery must not do with it is
/// turn it into a measured zero — a billed call booked as free.
const ANSWER_WITHOUT_USAGE: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
}}"#;

#[derive(Clone)]
struct Classifier {
    calls: Arc<AtomicUsize>,
}

impl Classifier {
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// A loopback classifier that counts every request it was sent.
///
/// The count is the whole of "recovery buys nothing": a repair that issued
/// another classifier request would move it, whatever the ledger then said.
async fn classifier_upstream(body: &'static str) -> (String, Classifier) {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::post;

    async fn handle(State(state): State<(Classifier, &'static str)>, _body: String) -> Response {
        state.0.calls.fetch_add(1, Ordering::SeqCst);
        Response::new(Body::from(state.1))
    }

    let state = Classifier {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/systemone", post(handle))
        .with_state((state.clone(), body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

fn config(base_url: &str) -> ClassifyConfig {
    let json = format!(
        r#"{{
          "enabled": true,
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

/// The same configuration on a **monthly** window, for the reset cases.
fn monthly_config(base_url: &str) -> ClassifyConfig {
    let mut config = config(base_url);
    config.budget.window = BudgetWindow::Monthly;
    config
}

fn env(name: &str) -> Option<String> {
    match name {
        "CLASSIFY_TEST_KEY" => Some("sk-classification-recovery-test".to_string()),
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

// ------------------------------------------------------------- ledger double

/// How a rigged settle fails, and **the difference is the whole suite**.
///
/// A backend error after the operation applied is not a rollback. These two
/// modes are the two sides of that: one leaves the ledger untouched and the
/// other leaves it already charged, and both hand the caller the same `Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailMode {
    /// The settle never reached the accounting. A repair is a *first* charge.
    BeforeApply,
    /// The settle applied and the acknowledgement was lost on the way back. A
    /// repair must deduplicate and charge nothing further.
    AfterApply,
    /// Everything lands. The control.
    Never,
}

/// An evaluation ledger that can lose one acknowledgement per call identity,
/// on either side of the accounting, and can be made to answer from a later
/// month.
struct RiggedLedger {
    inner: MemorySpendLedger,
    mode: Mutex<FailMode>,
    /// Identities that have already had their one failure. A repair is
    /// therefore always the attempt that succeeds, which is what makes the
    /// tests about *whether anything retries* rather than about the double.
    failed_once: Mutex<HashSet<String>>,
    settle_calls: AtomicUsize,
    grant_calls: AtomicUsize,
    /// Added to every `now_ms` handed to the inner ledger.
    ///
    /// The only way to cross a budget-window boundary deterministically: the
    /// repair reads the process clock, so the month has to move under it rather
    /// than the test waiting for one.
    skew_ms: AtomicU64,
    /// Every `(call, applied)` the inner ledger actually answered.
    applied: Mutex<Vec<(String, bool)>>,
}

impl RiggedLedger {
    fn new(mode: FailMode) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            mode: Mutex::new(mode),
            failed_once: Mutex::new(HashSet::new()),
            settle_calls: AtomicUsize::new(0),
            grant_calls: AtomicUsize::new(0),
            skew_ms: AtomicU64::new(0),
            applied: Mutex::new(Vec::new()),
        })
    }

    fn settle_calls(&self) -> usize {
        self.settle_calls.load(Ordering::SeqCst)
    }

    fn grant_calls(&self) -> usize {
        self.grant_calls.load(Ordering::SeqCst)
    }

    /// Move every later ledger operation `by` milliseconds into the future.
    fn skew(&self, by: u64) {
        self.skew_ms.store(by, Ordering::SeqCst);
    }

    fn now(&self, now_ms: u64) -> u64 {
        now_ms.saturating_add(self.skew_ms.load(Ordering::SeqCst))
    }

    /// Whether any settle of `call_id` was answered `applied: false` — the
    /// ledger saying "I already had this one", which is a successful
    /// acknowledgement and never a reason to charge again.
    fn deduplicated(&self, call_id: &str) -> bool {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .any(|(id, applied)| id == call_id && !applied)
    }

    /// Committed dollars for `principal` in the window `terms` names, read at
    /// the ledger's own (possibly skewed) clock.
    async fn committed_usd(&self, principal: &Principal, terms: &BudgetTerms) -> f64 {
        self.inner
            .balance(BalanceQuery {
                principal: principal.clone(),
                terms: terms.clone(),
                now_ms: self.now(roundhouse_core::now_ms()),
            })
            .await
            .expect("an in-memory ledger reads")
            .committed_usd
    }

    /// Commit spend that has nothing to do with the call under test, straight
    /// to the inner ledger.
    ///
    /// Account state to be preserved, not an exercise of the double: it goes
    /// around the failure modes deliberately, because what it is for is giving
    /// the project a committed balance that a later repair must not disturb.
    async fn commit_unrelated(&self, principal: &Principal, window: BudgetWindow, usd: f64) {
        self.inner
            .settle_grant(Settlement {
                principal: principal.clone(),
                key: SettlementKey::OncePerCall,
                response_id: ResponseId::new("unrelated_evaluation_call"),
                actual_usd: usd,
                window,
                now_ms: self.now(roundhouse_core::now_ms()),
            })
            .await
            .expect("an in-memory ledger settles");
    }
}

#[async_trait]
impl SpendLedger for RiggedLedger {
    async fn open_grant(&self, mut request: GrantRequest) -> Result<Grant, SpendError> {
        self.grant_calls.fetch_add(1, Ordering::SeqCst);
        request.now_ms = self.now(request.now_ms);
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, mut settlement: Settlement) -> Result<Settled, SpendError> {
        self.settle_calls.fetch_add(1, Ordering::SeqCst);
        settlement.now_ms = self.now(settlement.now_ms);
        let call_id = settlement.response_id.to_string();
        let mode = *self.mode.lock().unwrap();
        let first_attempt = self.failed_once.lock().unwrap().insert(call_id.clone());
        let fails = first_attempt && mode != FailMode::Never;

        if fails && mode == FailMode::BeforeApply {
            return Err(SpendError::Backend(anyhow::anyhow!(
                "simulated settlement failure before the ledger applied anything"
            )));
        }
        let settled = self.inner.settle_grant(settlement).await?;
        self.applied
            .lock()
            .unwrap()
            .push((call_id, settled.applied));
        if fails && mode == FailMode::AfterApply {
            // Applied, and the caller will never learn it. The one case a
            // record reading "uncommitted" would be actively false about.
            return Err(SpendError::Backend(anyhow::anyhow!(
                "simulated acknowledgement lost after the ledger applied it"
            )));
        }
        Ok(settled)
    }

    async fn balance(&self, mut query: BalanceQuery) -> Result<Balance, SpendError> {
        query.now_ms = self.now(query.now_ms);
        self.inner.balance(query).await
    }
}

// ------------------------------------------------------------------------ rig

/// One engine and the classification runtime wired to it, over a store and a
/// ledger the caller owns.
///
/// **Both are handed in**, which is what makes a restart expressible: dropping
/// this and building another over the same `Arc`s is a successor process
/// replaying a log while the durable ledger keeps its state. Nothing else in
/// this suite stands in for a restart.
struct Deployment<S: SessionStore = MemoryStore> {
    engine: Arc<Engine<S, ByteTokenizer>>,
    runtime: Arc<ClassificationRuntime<ByteTokenizer>>,
}

async fn deployment(
    store: &Arc<MemoryStore>,
    ledger: &Arc<RiggedLedger>,
    config: &ClassifyConfig,
) -> Deployment {
    deployment_over(store, Arc::clone(ledger) as Arc<dyn SpendLedger>, config).await
}

/// As [`deployment`], over any ledger double rather than only [`RiggedLedger`]
/// — the seam claim 2's overlap test needs, since its ledger gates one call
/// identity's `settle_grant` rather than failing it.
async fn deployment_over<S: SessionStore + Send + Sync + 'static>(
    store: &Arc<S>,
    ledger: Arc<dyn SpendLedger>,
    config: &ClassifyConfig,
) -> Deployment<S> {
    let runtime = compose("<test>", config, ledger, ByteTokenizer, &env)
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
            Arc::clone(store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
            catalog(),
            Arc::new(registry),
            Arc::new(AffinityPolicy::new()),
            EngineConfig {
                turn_deadline_ms: 5_000,
                ..common::config()
            },
        )
        // **A real local option, and the whole fixture rests on it.** Without a
        // fleet there is no local candidate at all, so a local-only policy
        // refuses the turn outright and an unrestricted one sends *every* turn
        // to the frontier — where every turn buys its own classification and
        // the ledger total stops being about the one call under test.
        .with_fleet(common::embedded_fleet().await as Arc<dyn LocalFleet>)
        .with_classifier(Arc::clone(&runtime)),
    );
    Deployment { engine, runtime }
}

/// An admission whose policy names no frontier target.
///
/// **Every turn after the first uses this, and the suite is unreadable
/// without knowing why.** `prepare` refuses a call whose decision admitted no
/// frontier target (`NotRun::NoAdmittedFrontier`), so a turn served under this
/// policy delivers results, drives repairs and classifies nothing. Without it
/// each later turn buys a classification of its own and the ledger total this
/// suite asserts on is the sum of an unknown number of unrelated calls — which
/// is exactly what the first run of these tests showed.
///
/// It is an ordinary configuration rather than a contrivance: a project whose
/// policy allows only local models is the shipped local-only posture.
fn local_only() -> Admission {
    admission_allowing("local/*")
}

/// An admission that can only route to the frontier provider this rig serves.
///
/// The partner of [`local_only`], and pinned for the same reason: with both a
/// local worker and a hosted model on offer, which one a cold affinity policy
/// prefers is a routing question this suite has no business depending on. The
/// one classified turn is the one that is *told* to reach a third party.
fn frontier_only() -> Admission {
    admission_allowing(&format!("{PROVIDER}/*"))
}

fn admission_allowing(pattern: &str) -> Admission {
    Admission {
        policy: Arc::new(TurnPolicy {
            min_quality: 0.0,
            allow: TargetFilter::parse([pattern]).expect("a valid filter"),
            frontier_cadence: None,
        }),
        ..Admission::open()
    }
}

impl<S: SessionStore + Send + Sync + 'static> Deployment<S> {
    /// A turn that classifies nothing. See [`local_only`].
    async fn turn(&self, session: &SessionId, turn: &str, text: &str) {
        self.turn_as(session, turn, text, &local_only()).await;
    }

    /// The one turn that reaches a frontier target, and so buys the single
    /// classification every test here recovers the settlement of.
    async fn classifying_turn(&self, session: &SessionId, turn: &str, text: &str) {
        self.turn_as(session, turn, text, &frontier_only()).await;
    }

    async fn turn_as(&self, session: &SessionId, turn: &str, text: &str, admission: &Admission) {
        self.engine.create_session(session).await.unwrap();
        self.engine
            .run_turn(
                session,
                TurnId::new(turn),
                vec![Item::user_text(text)],
                admission,
            )
            .await
            .expect("this fleet always answers");
    }

    /// Wait until the classification has finished and parked.
    async fn await_parked(&self, session: &SessionId) {
        for _ in 0..500 {
            if !self.runtime.ready(session).await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the classifier never answered");
    }
}

// --------------------------------------------------------------------- log

async fn events<S: SessionStore>(store: &Arc<S>, session: &SessionId) -> Vec<SessionEvent> {
    store
        .read_events(session, 0, 1_000)
        .await
        .expect("an in-memory log reads")
}

async fn results<S: SessionStore>(
    store: &Arc<S>,
    session: &SessionId,
) -> Vec<(u64, ClassificationRecord)> {
    events(store, session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ClassificationRecorded { record } => Some((event.seq, record)),
            _ => None,
        })
        .collect()
}

/// Every durable classification intent, with the sequence it landed at.
///
/// The cutoff half of "a repair is never a new call": an acknowledgement must
/// add no intent and move none of the intents already there.
async fn intents<S: SessionStore>(store: &Arc<S>, session: &SessionId) -> Vec<(u64, ResponseId)> {
    events(store, session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::ClassificationRequested { record } => {
                Some((event.seq, record.call_id))
            }
            _ => None,
        })
        .collect()
}

/// Every durable repair acknowledgement in the log, as the bytes carry it.
async fn repairs<S: SessionStore>(store: &Arc<S>, session: &SessionId) -> Vec<serde_json::Value> {
    events(store, session)
        .await
        .into_iter()
        .map(|event| serde_json::to_value(&event.kind).expect("an event serializes"))
        .filter(|value| value["type"] == REPAIR_TAG)
        .collect()
}

/// Append one already-unconfirmed settlement straight to the log: the intent
/// and its answered, unconfirmed result, with no classifier call and no turn
/// in between.
///
/// **Why not a classifying turn.** `repair_classification_settlements` runs
/// unconditionally at the end of *every* turn, so building more than one
/// unconfirmed settlement through the engine (turn, deliver, turn again)
/// races a background repair of the first against the delivery of the
/// second — the exact overlap [`overlapping_turns_must_not_duplicate_a_repair_still_in_flight`]
/// and [`a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn`] exist to
/// examine, not to accidentally trigger while setting up their premise. This
/// writes the same two events a real failed-before-application call leaves —
/// see [`a_session_with_an_unconfirmed_settlement`] — directly, so the
/// restarted deployment those tests build finds every settlement already
/// unrepaired and none of them touched yet.
async fn seed_unconfirmed_settlement(
    store: &MemoryStore,
    lease: &Lease,
    call_id: &str,
    turn_index: u64,
    usd: f64,
) {
    let source_response_id = ResponseId::new(format!("resp_src_{turn_index}"));
    let intent = ClassificationIntent {
        call_id: ResponseId::new(call_id),
        source_turn_index: turn_index,
        source_response_id: source_response_id.clone(),
        requested_at_ms: 1_000,
        expires_at_ms: 61_000,
        identity: ClassifierIdentity {
            model: "jev-1.12".into(),
            schema: "typesafe.systemone.choice.v1".into(),
            taxonomy_version: TAXONOMY_VERSION,
            projection_revision: 1,
            config_revision: 4,
        },
        reservation: ReservationRecord {
            rate_card: ProviderPricing {
                input_per_mtok_usd: 0.042,
                cached_input_per_mtok_usd: 0.0,
                cache_write_per_mtok_usd: 0.0,
                output_per_mtok_usd: 0.084,
            },
            estimated_input_tokens: 900,
            expected_output_tokens: 24,
            requested_usd: usd,
            hold_ttl_ms: 65_000,
            budget_limit_usd: 25.0,
            budget_window: BudgetWindow::Total,
            member_ceiling_usd: None,
            warn_at: 0.8,
        },
    };
    let record = ClassificationRecord {
        call_id: ResponseId::new(call_id),
        source_turn_index: turn_index,
        source_response_id,
        completed_at_ms: 2_000,
        outcome: ClassificationOutcome::Classified {
            classification: TurnClassification {
                taxonomy_version: TAXONOMY_VERSION,
                intent: Graded {
                    value: TurnIntent::Implement,
                    confidence: 0.8,
                },
                complexity: Graded {
                    value: TurnComplexity::Routine,
                    confidence: 0.6,
                },
                context_dependence: Graded {
                    value: ContextDependence::SelfContained,
                    confidence: 0.5,
                },
            },
            spend: EvaluationSpend::Measured {
                granted_usd: usd,
                usage: EvaluationUsage {
                    input_tokens: 300,
                    output_tokens: 40,
                },
                usd,
                settled: SettlementAck::Unconfirmed,
            },
            reported_model: Some("jev-1.12".into()),
        },
    };
    store
        .append_events(
            lease,
            vec![
                SessionEventKind::ClassificationRequested { record: intent },
                SessionEventKind::ClassificationRecorded { record },
            ],
        )
        .await
        .unwrap();
}

/// The one measured price the durable record holds, and the call it belongs to.
///
/// Read off the log rather than recomputed, which is the same rule the repair
/// itself is under: there is one pricing authority for a finished call and it
/// is the record.
async fn recorded_measured<S: SessionStore>(
    store: &Arc<S>,
    session: &SessionId,
) -> (ResponseId, f64) {
    let results = results(store, session).await;
    assert_eq!(results.len(), 1, "exactly one classification was recorded");
    let record = &results[0].1;
    let usd = match record.outcome.spend().expect("a call was attempted") {
        EvaluationSpend::Measured { usd, .. } => *usd,
        other => panic!("the classifier reported usage, so this must be measured: {other:?}"),
    };
    assert!(
        usd > 0.0,
        "the fixture must price to something to be about anything"
    );
    (record.call_id.clone(), usd)
}

/// Run turns on `deployment` until `want` dollars are committed, or give up.
///
/// A bounded poll rather than a sleep: recovery is background work started by a
/// turn, so the only honest wait is "keep serving and watch the ledger".
async fn drive_until_committed<S: SessionStore + Send + Sync + 'static>(
    deployment: &Deployment<S>,
    session: &SessionId,
    ledger: &RiggedLedger,
    principal: &Principal,
    terms: &BudgetTerms,
    want: f64,
    turns: &[&str],
) -> f64 {
    let mut committed = ledger.committed_usd(principal, terms).await;
    for turn in turns {
        deployment.turn(session, turn, "keep going").await;
        for _ in 0..100 {
            committed = ledger.committed_usd(principal, terms).await;
            if (committed - want).abs() < f64::EPSILON {
                return committed;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    committed
}

/// Serve turns until the durable repair acknowledgement is in the log.
async fn drive_until_repaired<S: SessionStore + Send + Sync + 'static>(
    deployment: &Deployment<S>,
    store: &Arc<S>,
    session: &SessionId,
    turns: &[&str],
) -> Vec<serde_json::Value> {
    let mut found = repairs(store, session).await;
    for turn in turns {
        deployment.turn(session, turn, "keep going").await;
        for _ in 0..100 {
            found = repairs(store, session).await;
            if !found.is_empty() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    found
}

/// As [`drive_until_repaired`], but counting only the acknowledgements naming
/// `call_id` — so a test with more than one unconfirmed settlement in flight
/// can wait for one specific call without the others racing it.
async fn drive_until_repaired_for<S: SessionStore + Send + Sync + 'static>(
    deployment: &Deployment<S>,
    store: &Arc<S>,
    session: &SessionId,
    call_id: &ResponseId,
    turns: &[&str],
) -> usize {
    let count = |found: &[serde_json::Value]| {
        found
            .iter()
            .filter(|value| value["record"]["call_id"] == call_id.to_string())
            .count()
    };
    let mut found = count(&repairs(store, session).await);
    for turn in turns {
        deployment.turn(session, turn, "keep going").await;
        for _ in 0..100 {
            found = count(&repairs(store, session).await);
            if found > 0 {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    found
}

/// A session whose classification call answered and whose settle failed, with
/// the result already delivered into the durable log.
///
/// Returns the store, the session, the call's identity and its measured price.
/// The deployment that produced it is **dropped before returning**: everything
/// after this point has only the log and the ledger, which is exactly the state
/// a restarted process finds.
async fn a_session_with_an_unconfirmed_settlement(
    store: &Arc<MemoryStore>,
    ledger: &Arc<RiggedLedger>,
    config: &ClassifyConfig,
    session: &SessionId,
) -> (ResponseId, f64) {
    let first = deployment(store, ledger, config).await;
    first
        .classifying_turn(session, "t1", "fix the parser")
        .await;
    first.await_parked(session).await;
    // `t2` is what delivers the parked result into the log through the engine's
    // own writer. Without it the record a repair reads would not exist.
    first.turn(session, "t2", "now add a test").await;

    let (call_id, usd) = recorded_measured(store, session).await;
    let results = results(store, session).await;
    assert!(
        results[0].1.outcome.committed_usd().is_none(),
        "the premise: nothing confirms this charge yet"
    );
    drop(first);
    (call_id, usd)
}

// --------------------------------------------------------------------- claims

/// **A settle that failed before the ledger applied it recovers its original
/// charge, after a restart, from the log alone.**
///
/// The classifier answered and billed; the settle never reached the accounting.
/// A successor process replaying this log has everything it needs — the call's
/// identity, its payer, the price the record holds and the window the intent
/// recorded — and must put the charge where the first process could not.
#[tokio::test]
async fn a_settlement_that_failed_before_application_recovers_after_a_restart() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_before_apply");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "the premise: the failed settle left the ledger untouched"
    );

    // The restart. A fresh engine and a fresh classification runtime over the
    // same persisted log and the same durable ledger.
    let restarted = deployment(&store, &ledger, &config).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed, usd,
        "the original measured price, recovered from the durable record"
    );
    assert_eq!(
        upstream.count(),
        1,
        "and recovered without buying the answer again"
    );
}

/// **An acknowledgement lost after the ledger applied the settle is resolved,
/// and resolving it charges nothing further.**
///
/// The failure the record cannot tell from the one above: the backend applied
/// the settle and then failed on the way back. Recovery must re-drive it — the
/// process genuinely does not know — and the ledger's `applied: false` is the
/// answer that resolves it. Treating that as a failure and charging again is
/// the one outcome worse than never repairing at all.
///
/// The load-bearing assertion is that the settle is **re-driven at all**.
/// Committed dollars already equal the price before recovery, so a test that
/// only checked the total would pass against a codebase that does nothing.
#[tokio::test]
async fn a_lost_acknowledgement_after_application_is_resolved_without_charging_twice() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::AfterApply);
    let session = SessionId::new("sess_lost_ack");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "the premise: the charge did land, and this process never learned it"
    );
    let settles_before = ledger.settle_calls();

    let restarted = deployment(&store, &ledger, &config).await;
    let found = drive_until_repaired(&restarted, &store, &session, &["t3", "t4"]).await;

    assert_eq!(
        found.len(),
        1,
        "the unconfirmed settlement is acknowledged exactly once in the log"
    );
    assert!(
        ledger.settle_calls() > settles_before,
        "the ledger must actually be asked again -- the record alone cannot \
         distinguish a lost acknowledgement from a charge that never landed"
    );
    assert!(
        ledger.deduplicated(call_id.as_str()),
        "and the ledger's answer must be `applied: false`: it already had this \
         call, which is a successful acknowledgement"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "one effective charge, not two"
    );
    assert_eq!(upstream.count(), 1, "and no second purchase");
}

/// **The recovered charge is the price the log recorded, not the price the
/// deployment charges now.**
///
/// The restarted process is configured with a rate card ten times the original.
/// A repair that re-derived the amount would commit ten times the money, and
/// every reconciliation afterwards would be comparing a historical call against
/// a current price list. There is one pricing authority for a finished call and
/// it is the durable record.
#[tokio::test]
async fn a_recovered_charge_is_not_repriced_by_a_later_rate_card() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let original = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_reprice");
    let principal = Principal::default_open();
    let terms = original.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &original, &session).await;

    // The operator edited the rate card between the call and the recovery.
    let mut dearer = config(&base_url);
    dearer.pricing.input_per_mtok_usd = original.pricing.input_per_mtok_usd * 10.0;
    dearer.pricing.output_per_mtok_usd = original.pricing.output_per_mtok_usd * 10.0;

    let restarted = deployment(&store, &ledger, &dearer).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed, usd,
        "the historical call keeps its historical price, whatever the live card \
         says now"
    );
    assert_eq!(upstream.count(), 1);
}

/// **Repeated replay after recovery charges once and buys nothing.**
///
/// Every turn of a session replays its whole log. Once the repair is durable,
/// further turns must find nothing to do — and even before it is durable, a
/// re-driven settle deduplicates rather than accumulating.
#[tokio::test]
async fn repeated_replay_after_recovery_charges_once_and_buys_no_second_call() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_replay");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;

    let restarted = deployment(&store, &ledger, &config).await;
    drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    // A second restart, and several more turns, over a log that now records the
    // repair. Nothing here may move the money again.
    drop(restarted);
    let again = deployment(&store, &ledger, &config).await;
    for turn in ["t5", "t6", "t7"] {
        again.turn(&session, turn, "and again").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "one effective charge across arbitrarily many replays"
    );
    assert_eq!(
        repairs(&store, &session).await.len(),
        1,
        "and one durable acknowledgement, not one per turn"
    );
    assert_eq!(upstream.count(), 1, "and one HTTP purchase, ever");
    assert_eq!(
        results(&store, &session).await.len(),
        1,
        "the classification record itself is untouched by its own repair"
    );
    assert!(
        ledger.deduplicated(call_id.as_str()) || ledger.settle_calls() >= 2,
        "either the repair was the first application and later replays \
         deduplicated, or nothing re-drove it at all -- this pins which"
    );
}

/// **A call whose usage the service never reported recovers as a release, and
/// never as a measured zero cost.**
///
/// The answer arrived without a usage block. What that call billed is exactly
/// what this deployment does not know, so the settle is a zero-dollar *release*
/// of the hold — and the durable record must go on saying the accounting is
/// unknown. A recovery that recorded a measured zero would book a billed call
/// as free, which is the one accounting lie the whole module exists to prevent.
#[tokio::test]
async fn missing_usage_recovers_as_a_release_and_never_as_a_measured_zero() {
    let (base_url, upstream) = classifier_upstream(ANSWER_WITHOUT_USAGE).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_no_usage");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let first = deployment(&store, &ledger, &config).await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;
    first.turn(&session, "t2", "now add a test").await;
    drop(first);

    let before = results(&store, &session).await;
    assert_eq!(before.len(), 1);
    assert!(
        matches!(
            before[0].1.outcome.spend().expect("a call was attempted"),
            EvaluationSpend::Unknown { .. }
        ),
        "the premise: the service reported no usage, so the spend is unknown"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    let found = drive_until_repaired(&restarted, &store, &session, &["t3", "t4"]).await;

    assert_eq!(found.len(), 1, "the release is acknowledged durably too");
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "a release commits nothing -- and this zero is the ledger's, not a \
         price anybody derived"
    );
    let after = results(&store, &session).await;
    assert!(
        matches!(
            after[0].1.outcome.spend().expect("a call was attempted"),
            EvaluationSpend::Unknown { .. }
        ),
        "and the record still says the accounting is unknown: a repair \
         acknowledges a settlement, it does not invent a measurement"
    );
    assert_eq!(upstream.count(), 1);
}

/// **A charge that never landed lands in the window that is open when it is
/// repaired.**
///
/// The monthly boundary rolled between the call and its recovery. The ledger
/// applies a realized amount at the clock it is handed, so this is a *first*
/// charge and it belongs to the window it is applied in. There are no
/// historical budget buckets here and a repair must not invent one.
#[tokio::test]
async fn a_first_delayed_charge_lands_in_the_window_it_is_repaired_in() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = monthly_config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_monthly_first");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;

    // Two months on. Everything the ledger does from here is in a later window.
    ledger.skew(62 * 24 * 60 * 60 * 1_000);
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "the new window opens empty"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed, usd,
        "a charge that had never been applied is applied now, in the window \
         that is open now"
    );
}

/// **A charge that already landed adds nothing to the window it is repaired
/// in.**
///
/// The partner of the test above, and the difference between them is the whole
/// of "distinguish a first delayed charge from a deduplicated old charge". Here
/// the settle applied in the *previous* month and only its acknowledgement was
/// lost. The settled-call record survives the reset by design, so the repair
/// deduplicates — and the new window must stay empty rather than inheriting a
/// charge that was already counted against a window that has closed.
#[tokio::test]
async fn a_deduplicated_old_charge_adds_nothing_to_the_window_it_is_repaired_in() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = monthly_config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::AfterApply);
    let session = SessionId::new("sess_monthly_dedup");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "the premise: the charge landed in this month"
    );

    ledger.skew(62 * 24 * 60 * 60 * 1_000);
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "and the window reset wiped it, which is what a window is"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    let found = drive_until_repaired(&restarted, &store, &session, &["t3", "t4"]).await;

    assert_eq!(found.len(), 1, "the acknowledgement is still recorded");
    assert!(
        ledger.deduplicated(call_id.as_str()),
        "the settled-call record outlives the window reset, so the repair is \
         recognized as the duplicate it is"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "a deduplicated old charge must not be re-applied into a new window: \
         it was counted once, against a window that has since closed"
    );
}

/// **The repaired charge is attributed to the payer the session recorded, and
/// never to whoever happens to be serving the turn that repairs it.**
///
/// The fixture drives a later turn under a different principal. **Every
/// supported surface makes that unreachable** — `ControlPlane::Configured`
/// prefixes a session id with `{project}/{user}/` and gates every turn on
/// `contains`, and `ControlPlane::Open` resolves one fixed principal for the
/// whole deployment — so this is deliberately an out-of-contract fixture rather
/// than a scenario a deployment can produce. What it pins is the *source* of
/// the payer: a repair that read the live admission would move a finished
/// call's money to a stranger, and that is a mistake no supported caller could
/// reveal.
#[tokio::test]
async fn a_repaired_charge_is_attributed_to_the_sessions_own_payer() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_payer");
    let original = Principal::default_open();
    let stranger = Principal::new("someone", "else");
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;

    let restarted = deployment(&store, &ledger, &config).await;
    let later = Admission {
        principal: stranger.clone(),
        ..local_only()
    };
    for turn in ["t3", "t4", "t5"] {
        restarted
            .turn_as(&session, turn, "a later turn", &later)
            .await;
        for _ in 0..50 {
            if ledger.committed_usd(&original, &terms).await == usd {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    assert_eq!(
        ledger.committed_usd(&original, &terms).await,
        usd,
        "the charge lands on the principal the session's own `SessionCreated` \
         recorded"
    );
    assert_eq!(
        ledger.committed_usd(&stranger, &terms).await,
        0.0,
        "and never on the principal of the turn that happened to drive the \
         repair"
    );
}

/// **Repairing opens no grant.**
///
/// A grant under an already-settled identity creates a hold that can never
/// settle and can only expire — it would sit against the project's ceiling for
/// a whole TTL for no reason. The original call opens exactly one; recovery
/// must add none.
#[tokio::test]
async fn recovery_opens_no_second_grant() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_no_regrant");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    let grants_before = ledger.grant_calls();
    assert_eq!(
        grants_before, 1,
        "the premise: the original call took exactly one hold"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        ledger.grant_calls(),
        grants_before,
        "recovery settles an existing identity and never reserves against it \
         again"
    );
}

/// **The control: a settlement the ledger confirmed is never repaired.**
///
/// Without this, every test above would also pass against an implementation
/// that repaired *everything* on every turn — which would be the same charge
/// re-driven forever, deduplicated by the ledger and invisible in the totals.
/// A confirmed settlement must leave no repair behind at all.
#[tokio::test]
async fn control_a_confirmed_settlement_is_never_repaired() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_control");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let first = deployment(&store, &ledger, &config).await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;
    first.turn(&session, "t2", "now add a test").await;
    let (_call_id, usd) = recorded_measured(&store, &session).await;
    assert_eq!(
        results(&store, &session).await[0].1.outcome.committed_usd(),
        Some(usd),
        "the premise: this one settled cleanly"
    );
    let settles_after_the_call = ledger.settle_calls();
    drop(first);

    let restarted = deployment(&store, &ledger, &config).await;
    for turn in ["t3", "t4", "t5"] {
        restarted.turn(&session, turn, "and again").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        repairs(&store, &session).await.is_empty(),
        "a confirmed settlement leaves nothing to repair"
    );
    assert_eq!(
        ledger.settle_calls(),
        settles_after_the_call,
        "and the evaluation ledger is not touched again at all -- a repair pass \
         that re-drove every settled call would be invisible in the totals and \
         unbounded in traffic"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "one charge, unchanged"
    );
    assert_eq!(upstream.count(), 1);
}

/// **The harness control.** The ledger double's two failure modes really do
/// differ in the one way the whole suite rests on: `BeforeApply` leaves nothing
/// committed and `AfterApply` leaves the charge in place, and both hand the
/// caller an error.
///
/// If this went red, every "recovered the charge" assertion above would be
/// about the double rather than about the engine.
#[tokio::test]
async fn control_the_two_failure_modes_differ_in_what_they_leave_committed() {
    let principal = Principal::default_open();
    let terms = config("http://127.0.0.1:1").budget_terms();
    let settlement = |id: &str| Settlement {
        principal: principal.clone(),
        key: SettlementKey::OncePerCall,
        response_id: ResponseId::new(id),
        actual_usd: 3.0,
        window: terms.budget.window,
        now_ms: 1_000,
    };

    let before = RiggedLedger::new(FailMode::BeforeApply);
    assert!(before.settle_grant(settlement("call_a")).await.is_err());
    assert_eq!(
        before.committed_usd(&principal, &terms).await,
        0.0,
        "BeforeApply leaves the accounting untouched"
    );

    let after = RiggedLedger::new(FailMode::AfterApply);
    assert!(after.settle_grant(settlement("call_a")).await.is_err());
    assert_eq!(
        after.committed_usd(&principal, &terms).await,
        3.0,
        "AfterApply has already charged, and the caller cannot tell"
    );
    // And a retry of the applied one deduplicates rather than charging again.
    let retry = after
        .settle_grant(settlement("call_a"))
        .await
        .expect("a duplicate settle is a valid no-op");
    assert!(
        !retry.applied,
        "the ledger recognizes the call it already had"
    );
    assert_eq!(after.committed_usd(&principal, &terms).await, 3.0);
}

// --------------------------------------------------------- overlap (claim 2)

/// A ledger double that gates `settle_grant` for exactly one call identity and
/// passes every other call straight through.
///
/// **What this is for.** `Engine::repair_classification_settlements` skips
/// starting a repair whose call id is already in `ready_repairs` (a *parked*
/// answer). It has no way to see a repair that is still *running* — one stuck
/// inside `settle_grant`, with no answer yet. This ledger holds exactly that
/// call open so a test can drive a second turn while the first repair is
/// genuinely still in flight, and count how many concurrent `settle_grant`
/// calls the gated identity actually saw.
struct GatedRepairLedger {
    inner: MemorySpendLedger,
    gated_call: String,
    hold: AtomicBool,
    gate: tokio::sync::Notify,
    /// Fires each time the gated call enters `settle_grant`, so a test can
    /// wait for the first worker to genuinely be inside it rather than
    /// guessing with a sleep.
    entered: tokio::sync::Notify,
    concurrent: AtomicUsize,
    peak_concurrent: AtomicUsize,
    attempts: AtomicUsize,
}

impl GatedRepairLedger {
    fn new(gated_call: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            gated_call: gated_call.to_string(),
            hold: AtomicBool::new(true),
            gate: tokio::sync::Notify::new(),
            entered: tokio::sync::Notify::new(),
            concurrent: AtomicUsize::new(0),
            peak_concurrent: AtomicUsize::new(0),
            attempts: AtomicUsize::new(0),
        })
    }

    /// Let every worker currently inside `settle_grant` for the gated call
    /// proceed, and every later one pass straight through.
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.gate.notify_waiters();
    }

    /// The most `settle_grant` calls for the gated identity that were ever
    /// inside it at once. `1` is the contract; anything higher is two workers
    /// racing the same settlement.
    fn peak_concurrent(&self) -> usize {
        self.peak_concurrent.load(Ordering::SeqCst)
    }

    /// How many times `settle_grant` was entered for the gated identity,
    /// concurrent or not — the count a duplicated *acknowledgement* (not just
    /// an overlapping attempt) would also move.
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for GatedRepairLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        if settlement.response_id.to_string() != self.gated_call {
            return self.inner.settle_grant(settlement).await;
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_concurrent.fetch_max(now, Ordering::SeqCst);
        self.entered.notify_one();
        while self.hold.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
        let settled = self.inner.settle_grant(settlement).await;
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        settled
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.inner.balance(query).await
    }
}

/// **A repair still running must not be duplicated by a second turn, and a
/// different call's repair must not be blocked by it.**
///
/// The engine's only overlap guard reads `ready_repairs` (engine.rs,
/// `repair_classification_settlements`) and skips a call id already
/// *parked*. A repair with no answer yet is in neither `ready_repairs` nor
/// any in-flight set the engine or the runtime tracks, so a second turn
/// arriving while the first repair is still inside `settle_grant` starts a
/// second worker for the identical call — which is a duplicate ledger round
/// trip at best, and, because `ClassificationRuntime::run_repair` pushes onto
/// `repaired` unconditionally, a second durable
/// `ClassificationSettlementRepaired` event for the same call at worst.
#[tokio::test]
async fn overlapping_turns_must_not_duplicate_a_repair_still_in_flight() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new("sess_overlap");

    // Two independent unconfirmed settlements, written directly rather than
    // bought through a classifying turn: `repair_classification_settlements`
    // runs unconditionally at the end of *every* turn (engine.rs), so driving
    // this session's premise through the engine would itself race a
    // background repair worker against the next setup turn -- the very
    // overlap this test exists to control. Seeding the log directly gives the
    // restarted deployment below both settlements already unrepaired, with no
    // engine turn and therefore no repair having run yet.
    let gated_call = "eval_overlap_gated";
    let other_call = "eval_overlap_other";
    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_overlap_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(Principal::default_open()),
                arm: None,
            }],
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(&store, &lease, gated_call, 1, 0.05).await;
    seed_unconfirmed_settlement(&store, &lease, other_call, 2, 0.07).await;
    store.release_lease(&lease).await.unwrap();

    let ledger = GatedRepairLedger::new(gated_call);
    let restarted =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;

    // t3 starts both repairs (capacity allows both); the gated one blocks
    // inside settle_grant, the other proceeds immediately.
    restarted.turn(&session, "t3", "keep going").await;
    tokio::time::timeout(Duration::from_secs(5), ledger.entered.notified())
        .await
        .expect("the repair must actually reach settle_grant to be gated at all");

    // Still stuck. A second turn arrives while nothing has parked yet.
    restarted.turn(&session, "t4", "keep going").await;

    // Give a duplicate worker, if one was started, the same chance the first
    // one had to reach the gate.
    for _ in 0..100 {
        if ledger.peak_concurrent() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        ledger.peak_concurrent(),
        1,
        "at most one worker may be inside settle_grant for the same call id \
         at once -- a second turn during the first repair's round trip must \
         not start a duplicate"
    );

    ledger.release();
    // The unblocked worker(s) finish in the background, independent of any
    // turn; wait for at least one answer to park before driving the turns
    // that deliver it, rather than racing a fixed sleep against them.
    for _ in 0..200 {
        if !restarted.runtime.ready_repairs(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let found = drive_until_repaired(&restarted, &store, &session, &["t5", "t6"]).await;
    let gated_repairs = found
        .iter()
        .filter(|value| value["record"]["call_id"] == gated_call)
        .count();
    assert_eq!(
        gated_repairs, 1,
        "one durable acknowledgement per call, not one per duplicate worker"
    );
    assert_eq!(
        ledger.attempts(),
        1,
        "and the ledger was asked about this call exactly once"
    );

    // The other call must not have been starved by the gate.
    let other_found = drive_until_repaired_for(
        &restarted,
        &store,
        &session,
        &ResponseId::new(other_call),
        &["t7", "t8"],
    )
    .await;
    assert_eq!(
        other_found, 1,
        "a different call's repair must still progress while the gated one \
         is held"
    );
    assert_eq!(
        upstream.count(),
        0,
        "a repair never reaches the classifier -- there is no answer to buy"
    );
}

// --------------------------------------------------- durable append (claim 3)

/// A store that refuses to append a durable repair acknowledgement while
/// armed, and delegates everything else to a real [`MemoryStore`].
///
/// The engine-level partner of `RefusingStore` in `tests/classification_runtime.rs`
/// (out of scope here, so not reused directly): that double fails a
/// classification's own append; this one fails the *settlement repair's*
/// append instead — the seam
/// `Engine::deliver_settlement_repairs` names explicitly ("An acknowledgement
/// whose append fails stays with the runtime for a later turn to deliver").
struct AckRefusingStore {
    inner: MemoryStore,
    refusing: AtomicBool,
    /// Appends actually refused. Without it "no acknowledgement reached the
    /// log" is equally the signature of a deployment that never attempted one,
    /// and the whole test would pass against a repair path that did nothing.
    refusals: AtomicUsize,
}

impl AckRefusingStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            refusing: AtomicBool::new(false),
            refusals: AtomicUsize::new(0),
        })
    }

    fn refuse(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::SeqCst);
    }

    fn refusals(&self) -> usize {
        self.refusals.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionStore for AckRefusingStore {
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
        let refuses = self.refusing.load(Ordering::SeqCst)
            && kinds.iter().any(|kind| {
                matches!(
                    kind,
                    SessionEventKind::ClassificationSettlementRepaired { .. }
                )
            });
        if refuses {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            return Err(StoreError::Backend(anyhow::anyhow!(
                "the store refused this repair acknowledgement append"
            )));
        }
        self.inner.append_events(lease, kinds).await
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

/// **A repair acknowledgement whose durable append fails stays retryable: no
/// second purchase, no double charge, no change to what the session can
/// classify, and no repair lost.**
///
/// The failure this test drives is different from every other test in this
/// suite: those fail the *ledger's* settle; this one succeeds at the ledger
/// (the repair genuinely resolves the settlement) and fails only at writing
/// `ClassificationSettlementRepaired` into the session's own log. Per
/// `Engine::deliver_settlement_repairs`, that failure must leave the
/// acknowledgement with the runtime (`acknowledge_repairs` is only called with
/// what was actually written) rather than lost, so a later turn -- once the
/// store recovers -- delivers it exactly once.
#[tokio::test]
async fn a_failed_repair_acknowledgement_append_stays_retryable() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = AckRefusingStore::new();
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_ack_append_fails");
    let principal = Principal::default_open();
    let terms = config.budget_terms();
    let call_id = "eval_ack_append_fails";

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_ack_append_fails", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(principal.clone()),
                arm: None,
            }],
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(&store.inner, &lease, call_id, 1, 0.05).await;
    store.release_lease(&lease).await.unwrap();

    // Armed before any turn runs, so the very first repair this deployment
    // produces is the one whose append fails.
    store.refuse(true);
    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;

    // The repair itself succeeds at the ledger on this turn -- only the log
    // append of its acknowledgement is refused.
    deployment.turn(&session, "t1", "keep going").await;
    for _ in 0..200 {
        if ledger.committed_usd(&principal, &terms).await > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.05,
        "the premise: the ledger actually applied the repair"
    );
    assert!(
        repairs(&store, &session).await.is_empty(),
        "and the durable acknowledgement of it did not make it into the log -- \
         the store refused that append"
    );

    // What the session could classify with, taken while the acknowledgement is
    // still missing. Every assertion about availability below is against this
    // rather than against a count, because a repair must move neither which
    // classifications exist nor where in the log they sit.
    let availability = results(&store, &session).await;
    let calls = intents(&store, &session).await;
    assert_eq!(
        availability.len(),
        1,
        "the one classification this log holds"
    );

    // Wait for the background repair before recording the ledger call count.
    for _ in 0..200 {
        if deployment.runtime.retained_repairs(&session).await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let settle_calls_before = ledger.settle_calls();
    let repaired_at_before = deployment
        .runtime
        .ready_repairs(&session)
        .await
        .first()
        .map(|delivery| delivery.record.repaired_at_ms)
        .expect("the repair parked before any refusing turn ran");

    // Further turns while the store is still refusing must not re-drive the
    // ledger (it already applied) or buy another call.
    deployment.turn(&session, "t2", "keep going").await;
    deployment.turn(&session, "t3", "keep going").await;
    assert!(
        store.refusals() > 0,
        "the delivery path must actually have attempted the append and been \
         refused -- an empty log is otherwise equally what a deployment that \
         never tried would leave"
    );
    // Allow a background retry to reach the ledger before comparing counts.
    let mut settle_calls_after = ledger.settle_calls();
    for _ in 0..50 {
        if settle_calls_after != settle_calls_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        settle_calls_after = ledger.settle_calls();
    }
    assert_eq!(
        settle_calls_after, settle_calls_before,
        "a refused append must not read back as an eviction that re-drives \
         the ledger for a settlement it already answered"
    );
    assert_eq!(
        deployment
            .runtime
            .ready_repairs(&session)
            .await
            .first()
            .map(|delivery| delivery.record.repaired_at_ms),
        Some(repaired_at_before),
        "the same parked acknowledgement across both refusing turns, not one \
         lost and silently recreated"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.05,
        "no double charge while the append keeps failing"
    );
    assert_eq!(upstream.count(), 0, "and no classifier call, ever");

    // The store recovers. The parked acknowledgement -- never lost, because
    // `acknowledge_repairs` was never told it was written -- lands now.
    store.refuse(false);
    let found = drive_until_repaired(&deployment, &store, &session, &["t4", "t5"]).await;
    assert_eq!(
        found.len(),
        1,
        "the repair is written exactly once, once the store accepts it"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.05,
        "still one effective charge"
    );
    assert_eq!(
        results(&store, &session).await,
        availability,
        "and the classification's own availability is untouched by any of \
         this: the same records, at the same sequence numbers, so what a later \
         turn may name and when it became nameable are both unmoved"
    );
    assert_eq!(
        intents(&store, &session).await,
        calls,
        "the repair is an acknowledgement and never a new call: no intent \
         joined the log for it"
    );

    // Replays after recovery must not re-drive the already-durable repair.
    let settles_before = ledger.settle_calls();
    deployment.turn(&session, "t6", "keep going").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        ledger.settle_calls(),
        settles_before,
        "a durably acknowledged repair is not driven again"
    );
    assert_eq!(upstream.count(), 0, "a repair never reaches the classifier");
}

// -------------------------------------------------- backlog scheduling (claim 4)

/// As [`config`], with a caller-chosen `max_in_flight` — so a backlog test can
/// make the admission ceiling small enough to observe without waiting out a
/// large one.
fn config_with_max_in_flight(base_url: &str, max_in_flight: usize) -> ClassifyConfig {
    let mut config = config(base_url);
    config.executor.max_in_flight = max_in_flight;
    config
}

/// **A long outage backlog is drained at a bounded rate per turn, not all at
/// once.**
///
/// Twenty unconfirmed settlements in one session -- far more than the
/// `max_in_flight` of 3 this deployment runs with -- seeded directly (see
/// [`seed_unconfirmed_settlement`] for why not through the engine). One turn
/// must not start more repairs than the admission ceiling allows; the rest
/// stay in the log for a later turn, which is what keeps a recovering
/// deployment's own serving turns cheap regardless of how large the backlog
/// that produced them was.
///
/// The sibling claim -- that a turn does not copy the whole backlog to decide
/// what to schedule -- is
/// [`a_long_outage_backlog_bounds_what_a_turn_retains_and_copies`].
#[tokio::test]
async fn a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn() {
    const BACKLOG: u64 = 20;
    const MAX_IN_FLIGHT: usize = 3;

    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config_with_max_in_flight(&base_url, MAX_IN_FLIGHT);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_backlog");
    let principal = Principal::default_open();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_backlog_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(principal.clone()),
                arm: None,
            }],
        )
        .await
        .unwrap();
    for i in 1..=BACKLOG {
        seed_unconfirmed_settlement(&store, &lease, &format!("eval_backlog_{i}"), i, 0.01).await;
    }
    store.release_lease(&lease).await.unwrap();

    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    assert_eq!(
        results(&store, &session).await.len(),
        BACKLOG as usize,
        "the premise: the whole backlog is durably unconfirmed before any turn \
         runs"
    );

    deployment.turn(&session, "t1", "keep going").await;
    // The turn itself only spawns workers; give them a bounded window to
    // actually reach the ledger before reading how many did.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settled_by_t1 = ledger.settle_calls();
    assert!(
        settled_by_t1 <= MAX_IN_FLIGHT,
        "one turn must start at most `max_in_flight` repairs against a \
         backlog far larger than that -- got {settled_by_t1} settle_grant \
         calls after a single turn with a ceiling of {MAX_IN_FLIGHT}"
    );
    assert!(
        settled_by_t1 > 0,
        "and it must start at least one -- an outage backlog that never \
         drains is a different defect this assertion would otherwise hide"
    );

    // A second turn, once the first batch's acknowledgements have had a
    // chance to land, admits roughly another `max_in_flight` worth of work --
    // bounded progress, not a stall and not the rest of the backlog at once.
    drive_until_repaired(&deployment, &store, &session, &["t2"]).await;
    deployment.turn(&session, "t3", "keep going").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settled_by_t3 = ledger.settle_calls();
    assert!(
        settled_by_t3 <= 3 * MAX_IN_FLIGHT,
        "three turns must not have exceeded three turns' worth of admitted \
         repairs -- got {settled_by_t3} settle_grant calls against a ceiling \
         of {MAX_IN_FLIGHT} per turn"
    );
    assert!(
        settled_by_t3 > settled_by_t1,
        "and real progress must still be happening on the third turn"
    );
    assert_eq!(upstream.count(), 0, "a repair never reaches the classifier");
}

/// **What a turn retains, and what it copies, are both bounded by admission --
/// not by the size of the backlog the outage left.**
///
/// The partner of the test above, and the half it could not reach. That one
/// bounds how many repairs a turn *starts*; this one bounds the state they
/// leave behind and the work a later turn does over it. Both are the same
/// property in the end: an acknowledgement holds its permit until it is
/// committed, so `max_in_flight` caps how many can be outstanding at once, and
/// the scheduling path no longer reads them at all -- a turn takes one copy of
/// what it is about to write and none to decide what to start.
///
/// Measured through `ClassificationRuntime::repair_handles_issued`, a counter
/// of every retained acknowledgement handed to a turn, rather than through a
/// clock: a wall-time assertion on this box would be about the box.
///
/// The last assertion is the one that keeps the first two honest -- a backlog
/// that never drains would satisfy every bound here perfectly.
#[tokio::test]
async fn a_long_outage_backlog_bounds_what_a_turn_retains_and_copies() {
    const BACKLOG: u64 = 20;
    const MAX_IN_FLIGHT: usize = 3;

    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config_with_max_in_flight(&base_url, MAX_IN_FLIGHT);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_backlog_bounds");
    let principal = Principal::default_open();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_backlog_bounds_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(principal.clone()),
                arm: None,
            }],
        )
        .await
        .unwrap();
    for i in 1..=BACKLOG {
        seed_unconfirmed_settlement(&store, &lease, &format!("eval_bounds_{i}"), i, 0.01).await;
    }
    store.release_lease(&lease).await.unwrap();

    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    deployment.turn(&session, "t1", "keep going").await;

    // A window in which an unbounded implementation parks the whole backlog --
    // every repair this ledger answers immediately -- rather than a sleep
    // chosen to be long enough for a bound that already holds.
    let mut retained = 0;
    for _ in 0..100 {
        retained = retained.max(deployment.runtime.retained_repairs(&session).await);
        if retained > MAX_IN_FLIGHT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        retained <= MAX_IN_FLIGHT,
        "an acknowledgement nobody has committed is outstanding work, so a \
         backlog of {BACKLOG} must not leave more than the ceiling of \
         {MAX_IN_FLIGHT} of them retained at once -- saw {retained}"
    );
    assert!(
        retained > 0,
        "and the turn must actually have repaired something for this to be \
         about a bound rather than about an idle deployment"
    );

    // One turn's worth of copying, measured across exactly one turn.
    let copied_before = deployment.runtime.repair_handles_issued();
    deployment.turn(&session, "t2", "keep going").await;
    let copied = deployment.runtime.repair_handles_issued() - copied_before;
    assert!(
        copied <= MAX_IN_FLIGHT,
        "a turn copies the acknowledgements it is about to write and nothing \
         else -- {copied} entries were handed to one turn against a ceiling of \
         {MAX_IN_FLIGHT}"
    );

    // And the backlog really drains, a bounded batch at a time.
    let mut turns = 0;
    while repairs(&store, &session).await.len() < BACKLOG as usize && turns < 60 {
        deployment
            .turn(&session, &format!("t_drain_{turns}"), "keep going")
            .await;
        turns += 1;
    }
    assert_eq!(
        repairs(&store, &session).await.len(),
        BACKLOG as usize,
        "every settlement is acknowledged exactly once after {turns} turns -- \
         neither the admission ceiling nor the identity claim may strand one"
    );
    assert_eq!(upstream.count(), 0, "a repair never reaches the classifier");
}

// ---------------------------------------------- payer attribution (claim 5)

/// **The positive control for payer attribution: a configured tenant's own
/// money, recovered after a restart, lands on that tenant.**
///
/// The two tests beside this one are both negatives -- a stranger's principal
/// is not used, and a log with no principal is refused -- and a repair path
/// that simply never charged anyone would satisfy both. This one drives the
/// supported shape end to end: a session whose log records
/// `acme/ada` as its payer, turns served under that same principal (which is
/// what every supported `ControlPlane` produces for one session), and the
/// recovered charge on `acme/ada`'s own account rather than on the open
/// default this suite's other fixtures use.
#[tokio::test]
async fn a_configured_tenants_charge_is_recovered_onto_its_own_account() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("acme/ada/sess_tenant_payer");
    let payer = Principal::new("acme", "ada");
    let terms = config.budget_terms();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_tenant_payer_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(payer.clone()),
                arm: None,
            }],
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(&store, &lease, "eval_tenant_payer", 1, 0.05).await;
    store.release_lease(&lease).await.unwrap();

    let restarted =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    let admission = Admission {
        principal: payer.clone(),
        ..local_only()
    };
    for turn in ["t1", "t2", "t3"] {
        restarted
            .turn_as(&session, turn, "keep going", &admission)
            .await;
    }

    assert_eq!(
        ledger.committed_usd(&payer, &terms).await,
        0.05,
        "the recovered charge is on the tenant the log named"
    );
    assert_eq!(
        ledger
            .committed_usd(&Principal::default_open(), &terms)
            .await,
        0.0,
        "and nothing landed on the deployment-wide default account, which is \
         what a repair that ignored the recorded payer would reach for"
    );
    assert_eq!(
        repairs(&store, &session).await.len(),
        1,
        "one durable acknowledgement, on a supported control-plane shape"
    );
    assert_eq!(upstream.count(), 0, "and no classifier call, ever");
}

/// **A session whose log names no payer is never repaired -- not even under
/// a later turn carrying a perfectly good admission principal.**
///
/// `a_repaired_charge_is_attributed_to_the_sessions_own_payer` (above) proves
/// source-of-payer with an out-of-contract fixture -- a later turn under a
/// *different* principal, which no supported control plane can produce for
/// one session. This test proves the same source-of-payer property through a
/// state a supported deployment's own log can genuinely hold:
/// `SessionCreated { principal: None, .. }`, the "log older than tenancy"
/// case `Session::record_created`'s own doc comment names, reachable by any
/// log written before principals existed. `Engine::repair_classification_settlements`
/// reads `session.state().principal()` and refuses outright on `None`
/// (engine.rs) rather than falling back to the live turn's admission -- which
/// is exactly what this drives: an admission whose principal actually would
/// receive money if the fallback existed.
#[tokio::test]
async fn a_session_with_no_recorded_payer_is_never_repaired() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_no_payer");
    let live_turn_principal = Principal::new("someone", "would_be_charged");
    let terms = config.budget_terms();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_no_payer_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: None,
                arm: None,
            }],
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(&store, &lease, "eval_no_payer", 1, 0.05).await;
    store.release_lease(&lease).await.unwrap();

    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    let admission = Admission {
        principal: live_turn_principal.clone(),
        ..local_only()
    };
    for turn in ["t1", "t2", "t3"] {
        deployment
            .turn_as(&session, turn, "keep going", &admission)
            .await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        repairs(&store, &session).await.is_empty(),
        "no repair may run at all -- there is no supported payer to attribute \
         one to"
    );
    assert_eq!(
        ledger.committed_usd(&live_turn_principal, &terms).await,
        0.0,
        "and specifically not on the live turn's own principal, which is the \
         mistake a fallback to the admission would make"
    );
    assert_eq!(ledger.settle_calls(), 0, "the ledger is never even asked");
    assert_eq!(upstream.count(), 0, "and no classifier call, ever");
}

/// **The other harness control.** A local-only target must not be what silences
/// the classifier in these fixtures.
///
/// Every test above asserts `upstream.count() == 1` after several later turns.
/// That is only evidence about *recovery* if those later turns would otherwise
/// have been classified — so this pins that the rig's first turn does reach a
/// frontier target and does produce exactly one intent.
#[tokio::test]
async fn control_the_rig_classifies_its_first_turn_through_a_frontier_target() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_rig_control");

    let first = deployment(&store, &ledger, &config).await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;

    let intents: Vec<_> = events(&store, &session)
        .await
        .into_iter()
        .filter(|event| matches!(event.kind, SessionEventKind::ClassificationRequested { .. }))
        .collect();
    assert_eq!(intents.len(), 1, "exactly one durable intent");
    assert_eq!(upstream.count(), 1, "and exactly one purchase");

    let decisions: Vec<Target> = events(&store, &session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision.chosen),
            _ => None,
        })
        .collect();
    assert!(
        decisions.iter().any(|target| !target.is_local()),
        "the classified turn reached a frontier target, which is what permits \
         the call at all"
    );
}

/// **Production repairs under the window the original intent recorded, and
/// this test is a passing regression pin of that -- not evidence that a
/// "live window" repair is needed.**
///
/// The requirements this stage inherited are explicit that the recorded
/// window is the contract to preserve: `TypeSafeShadow::repair_settlement`
/// settles with `window: settlement.window`, the amount the intent carried at
/// call time, on the documented rule that the live rate card and a repair's
/// own window are both out of scope for the same reason ("a repaired charge
/// that disagreed with the charge it replaced is drift nobody can see without
/// reading both"). This test drives that production code path unchanged and
/// it passes: the $5 committed under `Total` after the operator switched
/// `classify.budget.window` survives the Monthly-window repair that follows
/// it.
///
/// **Why it survives is a fact about this fixture's touch order, not a
/// general property of replaying a stale window.**
/// `ProjectAccount::settle_time` rolls `committed_usd` to zero only when the
/// window it is handed computes a start strictly later than
/// `window_started_ms`'s current high-water mark -- and the classifying
/// turn's own grant, made under the *original* Monthly config before the
/// operator ever switched anything, is the first ledger touch this account
/// ever sees. That call already advances `window_started_ms` to the current
/// month. The later Total-mode $5 leaves it there (`Total` always computes a
/// start of `0`), so when the repair replays `Monthly` in the same calendar
/// month, `window_start_ms(Monthly, now)` equals the mark already set and
/// nothing resets. A account whose *first-ever* ledger touch were the Monthly
/// repair itself -- e.g. one that had only ever seen `Total`-window spend
/// before this repair -- would roll: see
/// [`open_question_a_repair_can_still_roll_an_account_whose_first_ledger_touch_it_is`],
/// a ledger-level demonstration of exactly that, reported as an open
/// question this stage does not resolve. Nothing here changes production.
#[tokio::test]
async fn a_repair_does_not_roll_a_live_account_onto_the_window_its_intent_recorded() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    // The call is made, and its intent recorded, under a *monthly* window.
    let monthly = monthly_config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_window_drift");
    let principal = Principal::default_open();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &monthly, &session).await;

    // The operator switches the evaluation budget to a total window, and the
    // project accrues spend under it.
    let total = config(&base_url);
    let terms = total.budget_terms();
    ledger
        .commit_unrelated(&principal, terms.budget.window, 5.0)
        .await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        5.0,
        "the premise: the project has committed spend in the window that is \
         open now"
    );

    let restarted = deployment(&store, &ledger, &total).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        5.0 + usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed,
        5.0 + usd,
        "the $5 committed under Total survives this repair's Monthly settle"
    );
}

/// **Open question, not a production defect this stage resolves: a repair
/// can still roll an account whose first-ever ledger touch it is.**
///
/// Unlike [`a_repair_does_not_roll_a_live_account_onto_the_window_its_intent_recorded`],
/// nothing here touches this account under `Monthly` before the repair does.
/// `Total`-mode spend lands first -- `window_start_ms(Total, _)` is always
/// `0`, so it leaves `window_started_ms` at its initial `0` -- and the
/// Monthly-window settle that follows is the account's first look at a
/// nonzero window start. `ProjectAccount::settle_time` reads that as the
/// window having rolled and zeroes `committed_usd` before applying the
/// settle.
///
/// A production sequence that reaches this: a project whose evaluation
/// spend has only ever been committed under `Total` (or under `Monthly` in
/// an earlier calendar month whose watermark a `Total`-only stretch since
/// then never revisited) gets its first `Monthly`-window repair. This is a
/// ledger-level demonstration to make that reachable and named, not an
/// engine-level reproduction and not a fix -- both the engine path and the
/// right resolution (window-mode changes are an operator action already
/// outside a repair's remit; whether repair should carry a mode marker, or
/// whether this is simply the existing window-change contract working as
/// specified) are for whoever picks this up next to decide.
#[tokio::test]
async fn open_question_a_repair_can_still_roll_an_account_whose_first_ledger_touch_it_is() {
    let ledger = RiggedLedger::new(FailMode::Never);
    let principal = Principal::default_open();
    let total_terms = config("http://127.0.0.1:1").budget_terms();
    let monthly_terms = monthly_config("http://127.0.0.1:1").budget_terms();

    ledger
        .commit_unrelated(&principal, total_terms.budget.window, 5.0)
        .await;
    assert_eq!(
        ledger.committed_usd(&principal, &total_terms).await,
        5.0,
        "the premise: this account's only ledger touch so far is under Total"
    );

    // The account's first-ever Monthly settle -- what a repair whose intent
    // recorded a Monthly window would send, if this were its first touch. A
    // distinct response id: `commit_unrelated` always names the same one, and
    // `SettlementKey::OncePerCall` would read a second call under it as the
    // same settle repeated rather than a fresh one.
    ledger
        .settle_grant(Settlement {
            principal: principal.clone(),
            key: SettlementKey::OncePerCall,
            response_id: ResponseId::new("repair_like_monthly_call"),
            actual_usd: 0.02,
            window: monthly_terms.budget.window,
            now_ms: roundhouse_core::now_ms(),
        })
        .await
        .expect("an in-memory ledger settles");

    let after_total = ledger.committed_usd(&principal, &total_terms).await;
    let after_monthly = ledger.committed_usd(&principal, &monthly_terms).await;
    assert_eq!(
        after_monthly, 0.02,
        "the Monthly settle itself always lands"
    );
    assert_eq!(
        after_total, 0.02,
        "and it rolled the account first: read back under Total (which never \
         itself triggers a reset) the balance is only the 0.02 the Monthly \
         settle added, not 5.02 -- the $5 is gone, lost to a reset the \
         Monthly settle caused as a side effect of applying. This is the open \
         question, demonstrated at the ledger, not asserted as desired \
         behavior"
    );
}

// --------------------------------------- fast-failure scheduling (claim 1)

/// An evaluation ledger whose every settle fails at once -- no gate, no
/// deferred single failure, the shape a backend that is simply *down*
/// produces, as opposed to [`RiggedLedger`]'s one deferred failure per call
/// id or [`GatedRepairLedger`]'s held-open round trip. Counts every attempt.
struct FastFailingLedger {
    attempts: AtomicUsize,
}

impl FastFailingLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            attempts: AtomicUsize::new(0),
        })
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for FastFailingLedger {
    async fn open_grant(&self, _request: GrantRequest) -> Result<Grant, SpendError> {
        unreachable!("a repair settles an existing hold and never opens one")
    }

    async fn settle_grant(&self, _settlement: Settlement) -> Result<Settled, SpendError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(SpendError::Backend(anyhow::anyhow!(
            "simulated ledger outage: every settle fails immediately"
        )))
    }

    async fn balance(&self, _query: BalanceQuery) -> Result<Balance, SpendError> {
        unreachable!("a repair reads no balance")
    }
}

/// Seed `backlog` independent unconfirmed settlements directly into a fresh
/// session's log, on the same rationale [`a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn`]
/// seeds directly rather than through the engine: `repair_classification_settlements`
/// runs at the end of every turn, including a setup turn, so building the
/// backlog through the engine would start repairing it before this test's
/// own turn does.
async fn seed_backlog(
    store: &MemoryStore,
    session: &SessionId,
    principal: &Principal,
    prefix: &str,
    backlog: u64,
) {
    store.create_session(session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(session, &format!("node_{prefix}_setup"), 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(principal.clone()),
                arm: None,
            }],
        )
        .await
        .unwrap();
    for i in 1..=backlog {
        seed_unconfirmed_settlement(store, &lease, &format!("eval_{prefix}_{i}"), i, 0.01).await;
    }
    store.release_lease(&lease).await.unwrap();
}

/// **Repair scheduling attempts within one engine turn, when every ledger
/// settle fails immediately, for two backlogs two orders of magnitude
/// apart.**
///
/// [`a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn`] proves the
/// bound holds when every repair *succeeds*: a completed repair parks its
/// acknowledgement and keeps its permit until a later turn commits it, so
/// nothing can recycle capacity inside that one turn's own scheduling loop --
/// the retention and delivery-handle counters that test and its sibling read
/// are both downstream of that same parked-and-held state. A *failed* repair
/// takes a different path: `ClassificationRuntime::run_repair` returns with
/// nothing parked on a ledger error, which drops both `Capacity` and the
/// identity claim the instant that worker's future ends, with no later turn
/// required to free either. Under a multi-threaded runtime and a ledger that
/// answers every settle instantly, a permit freed that way can be
/// re-acquired by the same turn's still-running `repair_classification_settlements`
/// loop before that loop has finished walking the rest of the backlog -- so
/// whether one turn's attempts stay at `max_in_flight` or run past it is a
/// scheduling question the admission semaphore alone does not decide. This
/// measures it rather than assuming either answer, over backlogs two orders
/// of magnitude apart so a size-dependent effect would show up as a
/// difference between them rather than being invisible at one size.
///
/// Measured through the ledger's own `settle_grant` entry count, which is a
/// 1:1 count of repair *attempts* here: every seeded call id is unique, so no
/// attempt is ever refused as a same-identity duplicate, and every attempt
/// that is not refused calls `settle_grant` exactly once
/// (`TypeSafeShadow::repair_settlement`). Counted only after the one turn
/// that started them has returned and the running total has stopped moving,
/// so this is work attributable to that turn's own scheduling loop and not to
/// a later drain -- nothing else in this test ever calls `deployment.turn`
/// again for either backlog.
///
/// **What makes the two assertions below a guarantee rather than an
/// observation.** This test measured the question and could not settle it: a
/// run that stayed at `max_in_flight` said only that this box had scheduled
/// that way. `ClassificationRuntime::repair_batch` now fixes one turn's
/// candidates before the loop starts, so the ceiling holds however the workers
/// interleave, and
/// `classify_runtime::tests::a_turn_considers_at_most_max_in_flight_settlements_however_large_the_backlog`
/// is the structural half of it. This test stays as the runtime half: it is
/// what proves the bound survives a real engine turn, real workers and a
/// ledger that recycles permits as fast as it can fail.
#[tokio::test(flavor = "multi_thread")]
async fn repair_scheduling_attempts_per_turn_under_fast_ledger_failures() {
    const MAX_IN_FLIGHT: usize = 3;

    async fn attempts_after_one_turn(backlog: u64, label: &str) -> usize {
        let (base_url, upstream) = classifier_upstream(ANSWER).await;
        let config = config_with_max_in_flight(&base_url, MAX_IN_FLIGHT);
        let store = Arc::new(MemoryStore::new());
        let ledger = FastFailingLedger::new();
        let session = SessionId::new(format!("sess_fastfail_{label}"));
        let principal = Principal::default_open();

        seed_backlog(&store, &session, &principal, label, backlog).await;

        let deployment =
            deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
        deployment.turn(&session, "t1", "keep going").await;

        // A plateau wait rather than a fixed sleep: workers fail near
        // instantly, so poll until the running total stops moving (or a
        // generous ceiling elapses) instead of guessing a duration.
        let mut last = ledger.attempts();
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let now = ledger.attempts();
            if now == last {
                break;
            }
            last = now;
        }
        let attempts = ledger.attempts();
        assert_eq!(
            upstream.count(),
            0,
            "a repair never reaches the classifier, backlog={backlog}"
        );
        assert!(
            attempts > 0,
            "the turn must have started at least one repair for this to be \
             about a bound rather than about an idle deployment, \
             backlog={backlog}"
        );
        assert!(
            attempts <= backlog as usize,
            "sanity ceiling: one turn cannot attempt more repairs than the \
             backlog contains, backlog={backlog}, attempts={attempts}"
        );
        attempts
    }

    let small = attempts_after_one_turn(20, "small").await;
    let large = attempts_after_one_turn(2000, "large").await;

    eprintln!(
        "repair_scheduling_attempts_per_turn_under_fast_ledger_failures: \
         max_in_flight={MAX_IN_FLIGHT}, backlog=20 -> {small} attempts, \
         backlog=2000 -> {large} attempts"
    );

    assert!(
        small <= MAX_IN_FLIGHT,
        "one turn attempted {small} repairs against a backlog of 20 and a \
         ceiling of {MAX_IN_FLIGHT} -- fast ledger failures let this turn's \
         own scheduling loop recycle capacity within itself"
    );
    assert!(
        large <= MAX_IN_FLIGHT,
        "one turn attempted {large} repairs against a backlog of 2000 and a \
         ceiling of {MAX_IN_FLIGHT} -- fast ledger failures let this turn's \
         own scheduling loop recycle capacity within itself"
    );
}
