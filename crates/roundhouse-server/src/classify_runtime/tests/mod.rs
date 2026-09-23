// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the executor holds, and for how long.
//!
//! Every assertion across this module's children is about a bound. The one
//! that matters most is `mailbox`'s
//! `a_delivery_handle_holds_capacity_after_its_map_entry_is_evicted`: a permit
//! parked beside the result rather than inside it frees capacity the moment the
//! map is swept, while a handle is still holding the bytes.
//!
//! Split by claim into `mailbox`, `deadline`, `deadline_timing`,
//! `repair_claims` and `repair_batch_and_identity` (server-7, PR 18 round 1)
//! to keep each file under 1000 lines; the fixtures below -- the loopback
//! upstreams, the runtime builders, and the fund/await helpers every claim
//! needs -- are shared through `use super::*;`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use roundhouse_core::classify::projection::PromptCapture;
use roundhouse_core::classify::{
    ClassificationOutcome, EvaluationSpend, EvaluationUsage, FundingRefusal, ProjectionCaps,
    SettlementAck, TurnIntent,
};
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Allocation, Budget, BudgetWindow, Exhaustion, MemorySpendLedger, Secret,
};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::Target;
use roundhouse_core::routing::ledger::ProviderPricing;
use roundhouse_fleet::typesafe::SystemOneLimits;

use super::*;
use crate::test_support::classification::ANSWER;
use crate::typesafe_shadow::ShadowConfig;

/// What the rate card in [`runtime`] and [`runtime_with_ledger`] makes of the
/// usage [`ANSWER`] reports: 312 input tokens at $1/Mtok and 48 output tokens
/// at $2/Mtok.
///
/// Written out rather than read back from the configuration, so a test that
/// asserts this number is asserting what the answer *cost* and not merely that
/// two copies of the same expression agree.
const REPORTED_USD: f64 = (312.0 * 1.0 + 48.0 * 2.0) / 1_000_000.0;

/// A loopback upstream that answers after `delay`, so a shutdown or an expiry
/// has something in flight to act on.
async fn upstream(delay: Duration) -> SocketAddr {
    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;

    let app = Router::new().route(
        "/systemone",
        post(move || async move {
            tokio::time::sleep(delay).await;
            Response::new(Body::from(ANSWER))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// The same upstream, recording how many requests were in it at once.
///
/// The peak is what a concurrency bound is, and counting arrivals would not be
/// it: four calls that each arrived after the last finished would look the same.
async fn counting_upstream(
    delay: Duration,
    peak: Arc<std::sync::atomic::AtomicUsize>,
    live: Arc<std::sync::atomic::AtomicUsize>,
) -> SocketAddr {
    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;
    use std::sync::atomic::Ordering as AtomicOrdering;

    let app = Router::new().route(
        "/systemone",
        post(move || {
            let (peak, live) = (Arc::clone(&peak), Arc::clone(&live));
            async move {
                let now = live.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                peak.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(delay).await;
                live.fetch_sub(1, AtomicOrdering::SeqCst);
                Response::new(Body::from(ANSWER))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn terms() -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd: 1_000.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::Refuse,
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    }
}

fn limits(max_in_flight: usize) -> RuntimeLimits {
    RuntimeLimits {
        max_in_flight,
        max_http_concurrency: 2,
        call_ttl_ms: 30_000,
        result_retention_ms: 900_000,
        sweep_interval_ms: 50,
    }
}

fn runtime(addr: SocketAddr, limits: RuntimeLimits) -> Arc<ClassificationRuntime<ByteTokenizer>> {
    let client = SystemOneClient::new(
        format!("http://{addr}"),
        SystemOneLimits {
            max_request_bytes: 64 * 1024,
            max_response_bytes: 16 * 1024,
            deadline_ms: 5_000,
        },
    )
    .unwrap();
    let shadow = TypeSafeShadow::new(
        client,
        ShadowConfig::new(
            "jev-1.12",
            ProviderPricing {
                input_per_mtok_usd: 1.0,
                cached_input_per_mtok_usd: 0.0,
                cache_write_per_mtok_usd: 0.0,
                output_per_mtok_usd: 2.0,
            },
            16,
            ProjectionCaps {
                max_prior_classifications: 4,
                max_prior_turns: 4,
                max_prompt_chars: 2_000,
                max_total_bytes: 8 * 1024,
            },
            1,
        ),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
    );
    Arc::new(ClassificationRuntime::new(
        shadow,
        terms(),
        TurnCredential::Stored(Secret::api_key("sk-runtime-test-key").unwrap()),
        limits,
    ))
}

fn frontier() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-opus-4".into(),
    }
}

fn session() -> SessionId {
    SessionId::new("sess_runtime")
}

/// Fund one call the way the engine does, and answer the capacity token with it.
async fn fund(
    runtime: &Arc<ClassificationRuntime<ByteTokenizer>>,
    call_id: &str,
) -> (Capacity, PreparedCall) {
    let capacity = runtime.capacity().expect("room for one");
    let capture = PromptCapture::of(
        &[Item::user_text("fix the parser")],
        &runtime.projection_caps(),
    );
    let prepared = runtime
        .prepare(
            ClassificationSource {
                principal: Principal::new("proj_runtime", "user_runtime"),
                session_id: session(),
                call_id: ResponseId::new(call_id),
                source_turn_index: 1,
                source_response_id: ResponseId::new("resp_1"),
            },
            &capture,
            &[],
            &[],
            Some(&[frontier()]),
            roundhouse_core::now_ms(),
        )
        .expect("prepared");
    (capacity, prepared)
}

/// Wait until `session()` has `count` results parked, or fail the test.
async fn await_ready(
    runtime: &Arc<ClassificationRuntime<ByteTokenizer>>,
    count: usize,
) -> Vec<Delivered<ClassificationRecord>> {
    for _ in 0..200 {
        let ready = runtime.ready(&session()).await;
        if ready.len() >= count {
            return ready;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no classification result arrived");
}

mod deadline;
mod deadline_timing;
mod mailbox;
mod repair_batch_and_identity;
mod repair_claims;

/// A counting upstream that records every request it received, for a test
/// that must prove a *count*, not merely peak concurrency.
async fn counted_upstream() -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let app = Router::new().route(
        "/systemone",
        post(move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Response::new(Body::from(ANSWER))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, calls)
}

// ------------------------------------------------------- combined deadline (H?)
//
// Contract: the call's one absolute `expires_at_ms`, taken at submission,
// binds the queue wait and the HTTP send *together* -- not as two independent
// budgets. A test that only starves the HTTP queue proves the queue is
// bounded; it does not prove the send that follows is bounded by what is
// *left* of the same deadline rather than a fresh one. A separate question:
// whether a stalled grant/settlement is bound by that deadline at all.

/// A one-shot manual gate: a request that reaches it notifies `entered` and
/// then blocks until [`Self::open`] releases it. Full control over exactly
/// when a held request's response lands, with no dependency on timing.
struct Gate {
    release: tokio::sync::Notify,
    held: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            release: tokio::sync::Notify::new(),
            held: std::sync::atomic::AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
        })
    }

    fn open(&self) {
        self.held.store(false, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

/// An upstream whose Nth request is held by `gates[N]`, in arrival order.
///
/// Manual release rather than a fixed sleep, so a test proves what actually
/// ends a wait (a deadline firing on the client side, cancelling the request)
/// rather than racing a guessed duration against one.
async fn gated_upstream(gates: Vec<Arc<Gate>>) -> SocketAddr {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::post;

    #[derive(Clone)]
    struct GateState {
        gates: Arc<Vec<Arc<Gate>>>,
        next: Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn handle(State(state): State<GateState>) -> Response {
        let index = state.next.fetch_add(1, Ordering::SeqCst);
        let gate = Arc::clone(&state.gates[index]);
        gate.entered.notify_one();
        while gate.held.load(Ordering::SeqCst) {
            gate.release.notified().await;
        }
        Response::new(Body::from(ANSWER))
    }

    let state = GateState {
        gates: Arc::new(gates),
        next: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/systemone", post(handle))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// As [`runtime`], over a caller-supplied ledger rather than a fresh
/// [`MemorySpendLedger`] -- so a test can observe what a slow or stalled
/// evaluation ledger does to the call it is funding.
fn runtime_with_ledger(
    addr: SocketAddr,
    limits: RuntimeLimits,
    ledger: Arc<dyn roundhouse_core::control::SpendLedger>,
) -> Arc<ClassificationRuntime<ByteTokenizer>> {
    let client = SystemOneClient::new(
        format!("http://{addr}"),
        SystemOneLimits {
            max_request_bytes: 64 * 1024,
            max_response_bytes: 16 * 1024,
            deadline_ms: 5_000,
        },
    )
    .unwrap();
    let shadow = TypeSafeShadow::new(
        client,
        ShadowConfig::new(
            "jev-1.12",
            ProviderPricing {
                input_per_mtok_usd: 1.0,
                cached_input_per_mtok_usd: 0.0,
                cache_write_per_mtok_usd: 0.0,
                output_per_mtok_usd: 2.0,
            },
            16,
            ProjectionCaps {
                max_prior_classifications: 4,
                max_prior_turns: 4,
                max_prompt_chars: 2_000,
                max_total_bytes: 8 * 1024,
            },
            1,
        ),
        ledger,
        ByteTokenizer,
    );
    Arc::new(ClassificationRuntime::new(
        shadow,
        terms(),
        TurnCredential::Stored(Secret::api_key("sk-runtime-test-key").unwrap()),
        limits,
    ))
}

// -------------------------------------------------- settlement repair bounds

/// A ledger whose `settle_grant` never answers, so a repair's own bound is the
/// only thing that can end it.
struct NeverAnsweringLedger;

#[async_trait::async_trait]
impl roundhouse_core::control::SpendLedger for NeverAnsweringLedger {
    async fn open_grant(
        &self,
        _request: roundhouse_core::control::GrantRequest,
    ) -> Result<roundhouse_core::control::Grant, roundhouse_core::control::SpendError> {
        unreachable!("a repair settles an existing hold and never opens one")
    }

    async fn settle_grant(
        &self,
        _settlement: roundhouse_core::control::Settlement,
    ) -> Result<roundhouse_core::control::Settled, roundhouse_core::control::SpendError> {
        std::future::pending().await
    }

    async fn balance(
        &self,
        _query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, roundhouse_core::control::SpendError> {
        unreachable!("a repair reads no balance")
    }
}

fn unconfirmed(usd: f64) -> UnconfirmedSettlement {
    unconfirmed_call("eval_repair", usd)
}

/// As [`unconfirmed`], under a caller-chosen call id -- so a test that drives
/// more than one repair at once can tell them apart in `ready_repairs` and in
/// `acknowledge_repairs`, rather than colliding on the one literal id.
fn unconfirmed_call(call_id: &str, usd: f64) -> UnconfirmedSettlement {
    UnconfirmedSettlement {
        call_id: ResponseId::new(call_id),
        usd,
        window: BudgetWindow::Total,
    }
}

/// Park one repair acknowledgement and hand back the handles a turn would
/// have. Fails the test rather than returning empty, so no caller can mistake
/// "the ledger never answered" for "the fix released the permit".
async fn park_repair(
    runtime: &Arc<ClassificationRuntime<ByteTokenizer>>,
    session_id: &SessionId,
    call_id: &str,
) -> Vec<Delivered<ClassificationSettlementRepair>> {
    let capacity = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            capacity,
            session_id.clone(),
            Principal::default_open(),
            unconfirmed_call(call_id, REPORTED_USD),
        )
        .await;
    for _ in 0..300 {
        let parked = runtime.ready_repairs(session_id).await;
        if !parked.is_empty() {
            return parked;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the repair never produced an acknowledgement for {call_id}");
}

/// A ledger that holds one call identity inside `settle_grant` until it is
/// released, passes every other identity straight through, and counts how many
/// times the held identity was entered.
struct OneCallGatedLedger {
    inner: MemorySpendLedger,
    gated_call: String,
    hold: AtomicBool,
    gate: tokio::sync::Notify,
    entered: AtomicUsize,
}

impl OneCallGatedLedger {
    fn new(gated_call: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            gated_call: gated_call.to_string(),
            hold: AtomicBool::new(true),
            gate: tokio::sync::Notify::new(),
            entered: AtomicUsize::new(0),
        })
    }

    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.gate.notify_waiters();
    }

    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl roundhouse_core::control::SpendLedger for OneCallGatedLedger {
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
        if settlement.response_id.to_string() != self.gated_call {
            return self.inner.settle_grant(settlement).await;
        }
        self.entered.fetch_add(1, Ordering::SeqCst);
        while self.hold.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
        self.inner.settle_grant(settlement).await
    }

    async fn balance(
        &self,
        query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, roundhouse_core::control::SpendError> {
        self.inner.balance(query).await
    }
}

/// A ledger that refuses the first settle of one call identity and answers
/// every attempt after it.
struct FailsOnceLedger {
    inner: MemorySpendLedger,
    failing_call: String,
    failed: AtomicBool,
    attempts: AtomicUsize,
}

impl FailsOnceLedger {
    fn new(failing_call: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            failing_call: failing_call.to_string(),
            failed: AtomicBool::new(false),
            attempts: AtomicUsize::new(0),
        })
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl roundhouse_core::control::SpendLedger for FailsOnceLedger {
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
        if settlement.response_id.to_string() != self.failing_call {
            return self.inner.settle_grant(settlement).await;
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if !self.failed.swap(true, Ordering::SeqCst) {
            return Err(roundhouse_core::control::SpendError::Backend(
                anyhow::anyhow!("the evaluation ledger is still recovering"),
            ));
        }
        self.inner.settle_grant(settlement).await
    }

    async fn balance(
        &self,
        query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, roundhouse_core::control::SpendError> {
        self.inner.balance(query).await
    }
}
