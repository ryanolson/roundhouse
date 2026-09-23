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
use roundhouse_core::control::{MemorySpendLedger, Secret, TurnCredential};
use roundhouse_core::event::{CacheReadSource, SessionEvent, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, DecisionRecord, Target};
use roundhouse_core::store::{Lease, MemoryStore, SessionStore, StoreError};
use roundhouse_fleet::typesafe::{SystemOneClient, SystemOneLimits};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierClients, FrontierError, FrontierQuote, FrontierStream,
    LocalFleet, StaticFrontierCatalog,
};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};
use roundhouse_server::test_support::classification::{
    Answering, ClassifierUpstream, PROVIDER, admission_allowing, classification_catalog,
    classify_config,
};
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

/// A loopback classifier that records every body it was sent.
async fn classifier_upstream() -> (String, ClassifierUpstream) {
    let upstream = ClassifierUpstream::start().await;
    let base_url = upstream.base_url.clone();
    (base_url, upstream)
}

/// The configuration a deployment writes, pointed at the loopback classifier.
fn config(base_url: &str, enabled: bool) -> ClassifyConfig {
    classify_config(base_url, |value| {
        value["enabled"] = serde_json::json!(enabled);
    })
}

fn env(name: &str) -> Option<String> {
    match name {
        "CLASSIFY_TEST_KEY" => Some("sk-classification-runtime-test".to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------- fleet

fn catalog() -> StaticFrontierCatalog {
    classification_catalog()
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
/// `classification_settlement_recovery.rs` too, and both now reach
/// [`admission_allowing`] rather than each keeping its own copy.
fn local_only() -> Admission {
    admission_allowing("local/*")
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

// Path-declared: a tests/*.rs file is its own crate root, so a plain
// `mod x;` here would look for tests/x.rs (and, unqualified, Cargo would
// auto-discover that as a second top-level test binary) rather than the
// file actually beside this one's own directory.
#[path = "classification_runtime/capacity.rs"]
mod capacity;
#[path = "classification_runtime/delivery.rs"]
mod delivery;
#[path = "classification_runtime/ledger_isolation.rs"]
mod ledger_isolation;
#[path = "classification_runtime/lifetime.rs"]
mod lifetime;
#[path = "classification_runtime/settlement_repair.rs"]
mod settlement_repair;

// -------------------------------------------------------- shared doubles
//
// `RefusingStore` and `StallingLedger` are `settlement_repair`'s and
// `ledger_isolation`'s own fixtures, but `capacity`'s own claims reuse both
// for a second purpose -- a gated ledger to hold several classifications in
// flight at once, and a store whose refused append must give back the permit
// it took -- so both stay here rather than in either claim module, reached
// by every submodule through `use super::*;`.

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

// `Delegating` is deliberately not `use`d in this file: fixtures throughout
// call methods directly on a concrete double (`store.create_session(..)`),
// and having both traits' same-named methods in scope at once would make
// those calls ambiguous (E0034). Fully qualifying the trait here avoids that
// without pushing disambiguation onto every call site instead.
#[async_trait]
impl roundhouse_core::store::doubles::Delegating for RefusingStore {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<roundhouse_core::store::LearningMark>,
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
            false => self.inner.append_events(lease, kinds, mark).await,
        }
    }
}

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

// -------------------------------------------------- instrumented store (F2, F3)

/// A `MemoryStore` wrapper instrumented for two review-fix regressions: how
/// many `append_events` calls a turn makes before its own `TurnStarted`
/// commit (server-2's batching claim), and stalling the first repair
/// acknowledgement's append after it has already landed durably, so a test
/// can cancel the turn between that append and the runtime acknowledgement
/// that was supposed to follow it (server-3's re-append claim). One wrapper
/// rather than two, because both are "watch what `append_events` does" and a
/// second near-identical double would be exactly the duplication this
/// milestone's own review is about.
struct InstrumentedStore {
    inner: MemoryStore,
    /// Every `append_events` call, in order: whether its batch contained a
    /// `TurnStarted` event. Position of the first `true` is how many calls
    /// landed before the turn's own commit.
    calls: std::sync::Mutex<Vec<bool>>,
    /// Armed by [`Self::stall_next_repair_append`]; disarmed (and consumed)
    /// the first time a batch containing `ClassificationSettlementRepaired`
    /// is appended. Off by default: only the cancellation test ever needs the
    /// store to fail to return.
    stall_next_repair: AtomicBool,
    /// Fires the instant a stalled append has landed in the inner store,
    /// before this call parks forever — what a test synchronizes a
    /// cancellation on, rather than guessing with a sleep.
    repair_appended: tokio::sync::Notify,
}

impl InstrumentedStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            calls: std::sync::Mutex::new(Vec::new()),
            stall_next_repair: AtomicBool::new(false),
            repair_appended: tokio::sync::Notify::new(),
        }
    }

    /// How many `append_events` calls landed before the first one whose batch
    /// contained a `TurnStarted` event.
    fn calls_before_turn_started(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .position(|&has_turn_started| has_turn_started)
            .expect("a TurnStarted call")
    }

    /// Forget every call observed so far, so a later measurement is not
    /// polluted by an earlier turn's own `SessionCreated` or `TurnStarted`
    /// commit.
    fn reset_calls(&self) {
        self.calls.lock().unwrap().clear();
    }

    /// The next batch containing a repair acknowledgement lands, and then
    /// this call never returns.
    fn stall_next_repair_append(&self) {
        self.stall_next_repair.store(true, Ordering::SeqCst);
    }
}

// See `RefusingStore`'s `Delegating` impl above for why the trait is fully
// qualified rather than `use`d in this file.
#[async_trait]
impl roundhouse_core::store::doubles::Delegating for InstrumentedStore {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<roundhouse_core::store::LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let has_turn_started = kinds
            .iter()
            .any(|kind| matches!(kind, SessionEventKind::TurnStarted { .. }));
        let has_repair = kinds.iter().any(|kind| {
            matches!(
                kind,
                SessionEventKind::ClassificationSettlementRepaired { .. }
            )
        });
        let result = self.inner.append_events(lease, kinds, mark).await;
        self.calls.lock().unwrap().push(has_turn_started);
        if has_repair && self.stall_next_repair.swap(false, Ordering::SeqCst) {
            self.repair_appended.notify_one();
            // The append already landed in `self.inner` above; this future
            // simply never resolves, which is what leaves the caller
            // suspended between the append and whatever it does next.
            std::future::pending::<()>().await;
        }
        result
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
