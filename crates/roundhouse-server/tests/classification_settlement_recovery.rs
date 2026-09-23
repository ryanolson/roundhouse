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
    Principal, Settled, Settlement, SettlementKey, SpendError, SpendLedger,
};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, ProviderPricing, Target};
use roundhouse_core::store::{Lease, MemoryStore, SessionStore, StoreError};
use roundhouse_fleet::LocalFleet;
use roundhouse_fleet::{FrontierClient, FrontierClients, StaticFrontierCatalog};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};
use roundhouse_server::test_support::classification::{
    ANSWER, ANSWER_WITHOUT_USAGE, Answering, ClassifierUpstream, PROVIDER, admission_allowing,
    classification_catalog, classify_config,
};
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

/// A loopback classifier that counts every request it was sent, answering
/// every one with `body`.
///
/// The count is the whole of "recovery buys nothing": a repair that issued
/// another classifier request would move it, whatever the ledger then said.
async fn classifier_upstream(body: &'static str) -> (String, ClassifierUpstream) {
    let upstream = ClassifierUpstream::answering(body).await;
    let base_url = upstream.base_url.clone();
    (base_url, upstream)
}

fn config(base_url: &str) -> ClassifyConfig {
    classify_config(base_url, |_| {})
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

fn catalog() -> StaticFrontierCatalog {
    classification_catalog()
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
            None,
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

// Path-declared, like `redis_recovery` below: a `tests/*.rs` file is its own
// crate root, so a plain `mod x;` here would look for `tests/x.rs` (and,
// unqualified, Cargo would auto-discover that as a second top-level test
// binary) rather than the file actually beside this one's own directory.
#[path = "classification_settlement_recovery/backlog_bounds.rs"]
mod backlog_bounds;
#[path = "classification_settlement_recovery/failure_modes.rs"]
mod failure_modes;
#[path = "classification_settlement_recovery/overlap_and_claims.rs"]
mod overlap_and_claims;
#[path = "classification_settlement_recovery/payer_attribution.rs"]
mod payer_attribution;

// Real Redis: the same before-apply/after-apply claims through the real
// backends `shared_backend::open` builds. Path-declared so this stays one
// module and not a second auto-discovered `tests/*.rs` binary.
#[path = "classification_settlement_recovery/redis_recovery.rs"]
mod redis_recovery;
