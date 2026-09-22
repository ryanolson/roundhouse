// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the executor holds, and for how long.
//!
//! Every assertion here is about a bound. The one that matters most is
//! [`a_delivery_handle_holds_capacity_after_its_map_entry_is_evicted`]: a permit
//! parked beside the result rather than inside it frees capacity the moment the
//! map is swept, while a handle is still holding the bytes.

use std::net::SocketAddr;
use std::sync::Arc;
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
use crate::typesafe_shadow::ShadowConfig;

const ANSWER: &str = r#"{"model":"jev-1.12","answers":{
  "intent":{"type":"choice","choice":"implement","probabilities":{"implement":0.5,"diagnose":0.2,"explain":0.1,"review":0.1,"operate":0.05,"unknown":0.05},"confidence":0.82},
  "complexity":{"type":"choice","choice":"involved","probabilities":{"trivial":0.1,"routine":0.2,"involved":0.5,"deep":0.1,"unknown":0.1},"confidence":0.61},
  "context_dependence":{"type":"choice","choice":"recent","probabilities":{"self_contained":0.2,"recent":0.5,"deep":0.2,"unknown":0.1},"confidence":0.55}
},"usage":{"input_tokens":312,"output_tokens":48}}"#;

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
                max_prompt_chars: 2_000,
                max_total_bytes: 8 * 1024,
            },
            1,
        )
        .enable(),
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
    let projection = runtime.projection(&capture, &[], &[]).expect("it fits");
    let prepared = runtime
        .prepare(
            Principal::new("proj_runtime", "user_runtime"),
            session(),
            ResponseId::new(call_id),
            1,
            ResponseId::new("resp_1"),
            &projection,
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
) -> Vec<Delivery> {
    for _ in 0..200 {
        let ready = runtime.ready(&session()).await;
        if ready.len() >= count {
            return ready;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no classification result arrived");
}

/// **The capacity check is the first thing, and a full queue answers at once.**
#[tokio::test]
async fn a_full_queue_refuses_immediately_and_fails_open() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(1));

    let held = runtime.capacity().expect("the one permit");
    assert_eq!(runtime.available_capacity(), 0);
    assert!(
        runtime.capacity().is_none(),
        "a full queue answers now rather than waiting; a turn is holding this \
         thread"
    );
    drop(held);
    assert!(runtime.capacity().is_some(), "and the permit comes back");
}

/// **One permit spans queueing, running, and completed-but-undelivered.**
///
/// The result has finished and nobody has drained it, and that is exactly the
/// state a bound released at completion would fail to count.
#[tokio::test]
async fn a_completed_undelivered_result_still_occupies_capacity() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(2));

    let (capacity, funded) = fund(&runtime, "call_1").await;
    runtime.spawn(capacity, session(), funded).await;
    let ready = await_ready(&runtime, 1).await;

    assert_eq!(ready.len(), 1);
    assert_eq!(
        ready[0]
            .record
            .outcome
            .classification()
            .expect("a classification")
            .intent
            .value,
        TurnIntent::Implement
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "the finished result is still held, so its permit is still spent"
    );
}

/// **The permit lives inside the result, so eviction frees nothing while a
/// delivery handle is out.**
///
/// The failing shape is the obvious one: park the permit beside the map entry
/// and drop it when the entry is swept. Capacity then returns while this
/// process is still holding the payload bytes, which is the bound that was
/// supposed to cover exactly this state.
#[tokio::test]
async fn a_delivery_handle_holds_capacity_after_its_map_entry_is_evicted() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(2));

    let (capacity, prepared) = fund(&runtime, "call_1").await;
    runtime.spawn(capacity, session(), prepared).await;
    let handles = await_ready(&runtime, 1).await;
    assert_eq!(runtime.available_capacity(), 1);

    // Evict the map entry while the handle above is still alive. Swept on the
    // *retention* clock, which is the one a finished result is held under — see
    // `an_idle_session_is_reclaimed_by_the_global_sweep`.
    let dropped = runtime.sweep(handles[0].retain_until_ms() + 1).await;
    assert_eq!(dropped, 1, "the entry really was evicted");
    assert!(
        runtime.ready(&session()).await.is_empty(),
        "and the map really is empty"
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "capacity must stay spent while a handle still owns the bytes"
    );

    drop(handles);
    assert_eq!(
        runtime.available_capacity(),
        2,
        "and comes back only when the last owner lets go"
    );
}

/// A drain leaves the entries in place until they are acknowledged, so an
/// append that failed does not lose its result.
#[tokio::test]
async fn a_result_survives_a_drain_that_is_never_acknowledged() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(2));

    let (capacity, funded) = fund(&runtime, "call_1").await;
    runtime.spawn(capacity, session(), funded).await;
    let first = await_ready(&runtime, 1).await;
    drop(first);

    let second = runtime.ready(&session()).await;
    assert_eq!(
        second.len(),
        1,
        "a drain nobody acknowledged leaves the result for the next turn"
    );

    runtime
        .acknowledge(&session(), &[ResponseId::new("call_1")])
        .await;
    assert!(
        runtime.ready(&session()).await.is_empty(),
        "and an acknowledgement is what removes it"
    );
    drop(second);
    assert_eq!(runtime.available_capacity(), 2);
}

/// An idle session's results are reclaimed by the sweep, without a turn.
///
/// The session that strands results is precisely the one with no next turn to
/// run a per-turn hook, so the reclamation cannot be one.
#[tokio::test]
async fn an_idle_session_is_reclaimed_by_the_global_sweep() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(2));

    let (capacity, prepared) = fund(&runtime, "call_1").await;
    let call_expires_at_ms = prepared.expires_at_ms();
    runtime.spawn(capacity, session(), prepared).await;
    let handles = await_ready(&runtime, 1).await;
    let retain_until_ms = handles[0].retain_until_ms();
    drop(handles);

    assert_eq!(runtime.available_capacity(), 1, "held before the sweep");
    // **The call's own expiry does not reclaim a finished result**, and that
    // separation is a fix rather than a nicety: retention counted from the call
    // expiry threw away every classification that completed near the end of its
    // life — the slow ones, which are the ones a bound is about — before any
    // turn could see them.
    assert!(retain_until_ms > call_expires_at_ms);
    assert_eq!(
        runtime.sweep(call_expires_at_ms + 1).await,
        0,
        "a finished result outlives the deadline for *making* the call"
    );
    assert_eq!(runtime.sweep(retain_until_ms - 1).await, 0, "not yet");
    assert_eq!(runtime.sweep(retain_until_ms + 1).await, 1);
    assert_eq!(
        runtime.available_capacity(),
        2,
        "an idle session's capacity comes back with no turn to reclaim it"
    );
}

/// **A call whose expiry passes before it is sent makes no call, and says so.**
///
/// Expiry binds the queue wait, not only the request: a permit that arrives
/// after the deadline buys a request nobody may still want.
///
/// The result is `Unfunded { Expired }` rather than nothing, and the difference
/// is knowledge. No grant was opened and no socket was touched, so this process
/// *knows* the call cost nothing — which is a stronger and different statement
/// from the one an intent with no result at all makes, and that one still means
/// "a process died and nobody can say".
#[tokio::test]
async fn a_call_that_expires_before_it_is_sent_is_recorded_as_unfunded() {
    let addr = upstream(Duration::from_millis(50)).await;
    let runtime = runtime(
        addr,
        RuntimeLimits {
            // Already over by the time the worker looks.
            call_ttl_ms: 1,
            ..limits(2)
        },
    );

    let (capacity, prepared) = fund(&runtime, "call_1").await;
    // Prepared, and then left until its life has run out — which is the state
    // this is about. Sleeping here rather than relying on the worker being slow
    // makes the expiry a fact of the clock rather than a race with the scheduler.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(prepared.expires_at_ms() < roundhouse_core::now_ms());
    runtime.spawn(capacity, session(), prepared).await;
    let ready = await_ready(&runtime, 1).await;

    assert_eq!(
        ready[0].record.outcome,
        ClassificationOutcome::Unfunded {
            reason: FundingRefusal::Expired
        }
    );
    assert!(
        ready[0].record.outcome.spend().is_none(),
        "nothing was sent, which is not the same as an unknown cost"
    );
    drop(ready);
    runtime
        .acknowledge(&session(), &[ResponseId::new("call_1")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "and its capacity is not stranded"
    );
}

/// **The HTTP limit binds independently of the admission limit.**
///
/// The two answer different questions — how much this process is holding, and
/// how hard it is leaning on one upstream — and a deployment that wanted more
/// of the first should not have to ask for more of the second. With one number
/// for both, this test could not fail; with the HTTP permit dropped early, the
/// observed peak would exceed one.
#[tokio::test]
async fn the_http_limit_bounds_concurrent_requests_below_the_admission_limit() {
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let addr = counting_upstream(
        Duration::from_millis(80),
        Arc::clone(&peak),
        Arc::clone(&live),
    )
    .await;
    let runtime = runtime(
        addr,
        RuntimeLimits {
            // Room for four at once, and one on the wire.
            max_in_flight: 4,
            max_http_concurrency: 1,
            ..limits(4)
        },
    );

    for index in 0..4 {
        let (capacity, funded) = fund(&runtime, &format!("call_{index}")).await;
        runtime.spawn(capacity, session(), funded).await;
    }
    assert_eq!(
        runtime.available_capacity(),
        0,
        "all four were admitted, so the admission limit is not what bounds the wire"
    );
    await_ready(&runtime, 4).await;

    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "four admitted calls must still reach the upstream one at a time"
    );
}

/// Shutdown aborts what is in flight, releases every permit, and refuses new
/// work.
#[tokio::test]
async fn shutdown_cancels_in_flight_work_and_releases_its_capacity() {
    let addr = upstream(Duration::from_secs(30)).await;
    let runtime = runtime(addr, limits(2));

    let (capacity, funded) = fund(&runtime, "call_1").await;
    runtime.spawn(capacity, session(), funded).await;
    assert_eq!(runtime.available_capacity(), 1, "one is in flight");

    runtime.shutdown().await;

    assert_eq!(
        runtime.available_capacity(),
        2,
        "an aborted worker drops its permit"
    );
    assert!(
        runtime.capacity().is_none(),
        "and a stopped runtime accepts no more work"
    );
    assert!(runtime.ready(&session()).await.is_empty());
}

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

/// **Capacity acquired before the lifetime ended must not license a request
/// sent after it.**
///
/// `fund` obtains a `Capacity` and a `PreparedCall` without spawning either --
/// standing in for the window between `Engine::run_turn` writing the durable
/// intent and handing the pair to `spawn`. `shutdown` then ends the runtime's
/// lifetime while that pair is held outside the runtime entirely, unknown to
/// the supervisor and the worker set. Submitting it only afterward is the
/// exact ordering `shutdown_cancels_in_flight_work_and_releases_its_capacity`
/// does not cover: that test spawns before shutting down, so it proves
/// cancellation of a task already on the wire, not a refusal of a capacity
/// token presented after the lifetime ended. `spawn`'s own `stopped` check is
/// what this is about, and the ordinary pre-shutdown dispatch is kept as a
/// positive control so the counting upstream is proven to actually count
/// before it is asked to prove an absence.
#[tokio::test]
async fn capacity_acquired_before_shutdown_makes_no_request_once_it_has_ended() {
    let (addr, calls) = counted_upstream().await;
    let runtime = runtime(addr, limits(2));

    // The positive control: an ordinary pre-shutdown dispatch still fires.
    let (control_capacity, control_call) = fund(&runtime, "control").await;
    runtime
        .spawn(control_capacity, session(), control_call)
        .await;
    await_ready(&runtime, 1).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the counting upstream must actually count for this test to be about \
         anything"
    );

    // Capacity acquired and a call prepared -- but not yet submitted -- before
    // the lifetime ends.
    let (late_capacity, late_call) = fund(&runtime, "late").await;
    assert_eq!(
        runtime.available_capacity(),
        0,
        "both permits are accounted for: one delivered and held, one in hand"
    );

    runtime.shutdown().await;

    // Submit the previously-admitted call only now, after the lifetime ended.
    runtime.spawn(late_capacity, session(), late_call).await;

    // Bounded wait, standing in for "long enough that a wrongly-dispatched
    // request would have reached the upstream".
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no classifier request may be sent for capacity presented after \
         shutdown -- the call count must still read exactly the control's"
    );
    assert!(
        runtime.ready(&session()).await.is_empty(),
        "and no result was parked for the late call either"
    );
    assert_eq!(
        runtime.available_capacity(),
        2,
        "the late call's permit is released by `spawn`'s early return, not \
         leaked"
    );
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
                max_prompt_chars: 2_000,
                max_total_bytes: 8 * 1024,
            },
            1,
        )
        .enable(),
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

/// A stalled send eventually times out after waiting for the HTTP permit.
/// This test checks timeout delivery and capacity release. The next test
/// distinguishes the original deadline from a fresh timeout after queueing.
#[tokio::test]
async fn the_absolute_deadline_binds_the_queue_wait_and_the_send_as_one() {
    let occupant_gate = Gate::new();
    // The server never answers, so completion must come from the deadline.
    let subject_gate = Gate::new();
    let addr = gated_upstream(vec![Arc::clone(&occupant_gate), Arc::clone(&subject_gate)]).await;

    let runtime = runtime(
        addr,
        RuntimeLimits {
            max_http_concurrency: 1,
            call_ttl_ms: 300,
            ..limits(2)
        },
    );

    let (occupant_capacity, occupant_call) = fund(&runtime, "occupant").await;
    runtime
        .spawn(occupant_capacity, session(), occupant_call)
        .await;
    tokio::time::timeout(Duration::from_secs(5), occupant_gate.entered.notified())
        .await
        .expect("the occupant must actually reach the upstream for this test to be about anything");

    let (subject_capacity, subject_call) = fund(&runtime, "subject").await;
    let subject_expires_at_ms = subject_call.expires_at_ms();
    runtime
        .spawn(subject_capacity, session(), subject_call)
        .await;

    // A controlled, known queue wait: most of the subject's 300ms life spent
    // waiting for the HTTP permit alone, before the occupant releases it.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        roundhouse_core::now_ms() < subject_expires_at_ms,
        "the subject must not already be expired -- the send's deadline is \
         what this test is about, not a pre-expired queue wait"
    );
    occupant_gate.open();

    let ready = tokio::time::timeout(Duration::from_secs(5), await_ready(&runtime, 2))
        .await
        .expect(
            "both calls must resolve well within the outer bound -- a hang \
             here means the subject's send was not cut off by its own \
             deadline",
        );
    let subject_record = ready
        .iter()
        .find(|delivery| delivery.record.call_id == ResponseId::new("subject"))
        .expect("the subject parked its result");
    let occupant_record = ready
        .iter()
        .find(|delivery| delivery.record.call_id == ResponseId::new("occupant"))
        .expect("the occupant parked its result");

    assert!(
        matches!(
            occupant_record.record.outcome,
            ClassificationOutcome::Classified { .. }
        ),
        "the occupant's own call must have actually completed for this test \
         to be meaningful: {:?}",
        occupant_record.record.outcome
    );
    match &subject_record.record.outcome {
        ClassificationOutcome::Failed { reason, spend } => {
            assert_eq!(
                reason, "deadline_exceeded",
                "the send must be cut off by the call's ORIGINAL absolute \
                 deadline once the queue wait consumed most of its life, not \
                 answered under a deadline reset when the HTTP permit finally \
                 freed"
            );
            assert!(
                matches!(
                    spend,
                    roundhouse_core::classify::EvaluationSpend::Unknown { .. }
                ),
                "nothing priceable came back, so the cost is unknown rather \
                 than measured or invented as free: {spend:?}"
            );
        }
        other => panic!(
            "expected the subject's send to time out against its original \
             deadline once the queue wait consumed most of its life, got \
             {other:?}"
        ),
    }
    assert!(
        subject_record.record.completed_at_ms >= subject_expires_at_ms,
        "completion is recorded at the moment the deadline actually fired, \
         not backdated to submission or to the end of the queue wait"
    );

    assert_eq!(
        runtime.available_capacity(),
        0,
        "both results are parked and undelivered, so both permits are still \
         held"
    );
    drop(ready);
    runtime
        .acknowledge(
            &session(),
            &[ResponseId::new("occupant"), ResponseId::new("subject")],
        )
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "capacity is fully reclaimed once delivered and acknowledged, the \
         same as any other completed call"
    );
}

/// Queue time consumes the call's deadline rather than starting a new allowance.
/// A 1000ms queue wait leaves 500ms of the 1500ms lifetime. The assertion allows
/// 500ms of scheduling delay but rejects the extra 1000ms a reset would grant.
/// This uses real time and can fail if scheduling delays exceed that tolerance.
#[tokio::test]
async fn a_reset_deadline_after_the_queue_wait_is_not_the_calls_own() {
    const CALL_TTL_MS: u64 = 1_500;
    const QUEUE_WAIT_MS: u64 = 1_000;
    const COMPLETION_SLACK_MS: u64 = 500;

    let occupant_gate = Gate::new();
    // Never opened, for the same reason as the sibling test: whatever ends
    // the subject's send is a deadline, not an answer.
    let subject_gate = Gate::new();
    let addr = gated_upstream(vec![Arc::clone(&occupant_gate), Arc::clone(&subject_gate)]).await;

    let runtime = runtime(
        addr,
        RuntimeLimits {
            max_http_concurrency: 1,
            call_ttl_ms: CALL_TTL_MS,
            ..limits(2)
        },
    );

    let (occupant_capacity, occupant_call) = fund(&runtime, "occupant").await;
    runtime
        .spawn(occupant_capacity, session(), occupant_call)
        .await;
    tokio::time::timeout(Duration::from_secs(5), occupant_gate.entered.notified())
        .await
        .expect("the occupant must actually reach the upstream for this test to be about anything");

    let (subject_capacity, subject_call) = fund(&runtime, "subject").await;
    let subject_expires_at_ms = subject_call.expires_at_ms();
    runtime
        .spawn(subject_capacity, session(), subject_call)
        .await;

    tokio::time::sleep(Duration::from_millis(QUEUE_WAIT_MS)).await;
    assert!(
        roundhouse_core::now_ms() < subject_expires_at_ms,
        "the subject must not already be expired before the occupant releases \
         the HTTP permit -- the send's own deadline is what this test is \
         about, not a queue wait that outran the call's whole life"
    );
    occupant_gate.open();

    // Polls past both candidate completion times (the call's own deadline and
    // a reset one) rather than reusing `await_ready`'s shorter, fixed window,
    // which is sized for this suite's near-instant calls and would time out
    // on the reset deadline before this test's own assertion ever ran.
    let poll_bound_ms = CALL_TTL_MS + QUEUE_WAIT_MS + COMPLETION_SLACK_MS + 2_000;
    let ready = tokio::time::timeout(Duration::from_millis(poll_bound_ms), async {
        loop {
            let ready = runtime.ready(&session()).await;
            if ready.len() >= 2 {
                return ready;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both calls must resolve well inside the outer bound");

    let subject_record = ready
        .iter()
        .find(|delivery| delivery.record.call_id == ResponseId::new("subject"))
        .expect("the subject parked its result");
    assert!(
        matches!(
            subject_record.record.outcome,
            ClassificationOutcome::Failed { .. }
        ),
        "expected the subject's send to time out, got {:?}",
        subject_record.record.outcome
    );

    let ceiling_ms = subject_expires_at_ms + COMPLETION_SLACK_MS;
    assert!(
        subject_record.record.completed_at_ms <= ceiling_ms,
        "the send must be cut off by the call's ORIGINAL absolute deadline \
         ({subject_expires_at_ms}ms), not by a fresh {CALL_TTL_MS}ms budget \
         started once the HTTP permit freed -- completed at {}ms, expected \
         at or before {ceiling_ms}ms",
        subject_record.record.completed_at_ms
    );

    drop(ready);
    runtime
        .acknowledge(
            &session(),
            &[ResponseId::new("occupant"), ResponseId::new("subject")],
        )
        .await;
}

/// An evaluation ledger whose `open_grant` can be held open indefinitely, so
/// a test can prove what a call's absolute deadline does -- and does not --
/// bind while a grant is stalled inside it.
struct GatedLedger {
    open_grant_gate: tokio::sync::Notify,
    open_grant_entered: tokio::sync::Notify,
    hold_open_grant: std::sync::atomic::AtomicBool,
    settle_gate: tokio::sync::Notify,
    settle_entered: tokio::sync::Notify,
    hold_settle: std::sync::atomic::AtomicBool,
    /// What a grant answers with, where the test needs that to be *less* than
    /// the ask. `None` grants the quote in full, which is what every case that
    /// is not about a partial reservation wants.
    grant_usd: Option<f64>,
}

impl GatedLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open_grant_gate: tokio::sync::Notify::new(),
            open_grant_entered: tokio::sync::Notify::new(),
            hold_open_grant: std::sync::atomic::AtomicBool::new(true),
            settle_gate: tokio::sync::Notify::new(),
            settle_entered: tokio::sync::Notify::new(),
            hold_settle: std::sync::atomic::AtomicBool::new(true),
            grant_usd: None,
        })
    }

    /// The same gates, over a ledger that answers every grant with `granted_usd`
    /// whatever was asked for -- so a test can reach the partial-reservation
    /// branch of `reserve`, which hands the short hold straight back through a
    /// zero-dollar settle before refusing.
    fn granting(granted_usd: f64) -> Arc<Self> {
        let mut ledger = Self::new();
        Arc::get_mut(&mut ledger).expect("sole owner").grant_usd = Some(granted_usd);
        ledger
    }

    fn release_open_grant(&self) {
        self.hold_open_grant.store(false, Ordering::SeqCst);
        self.open_grant_gate.notify_waiters();
    }

    fn release_settle(&self) {
        self.hold_settle.store(false, Ordering::SeqCst);
        self.settle_gate.notify_waiters();
    }
}

#[async_trait::async_trait]
impl roundhouse_core::control::SpendLedger for GatedLedger {
    async fn open_grant(
        &self,
        request: roundhouse_core::control::GrantRequest,
    ) -> Result<roundhouse_core::control::Grant, roundhouse_core::control::SpendError> {
        self.open_grant_entered.notify_one();
        while self.hold_open_grant.load(Ordering::SeqCst) {
            self.open_grant_gate.notified().await;
        }
        Ok(roundhouse_core::control::Grant {
            granted_usd: self.grant_usd.unwrap_or(request.requested_usd),
            state: roundhouse_core::control::LedgerState::Unconstrained,
        })
    }

    async fn settle_grant(
        &self,
        settlement: roundhouse_core::control::Settlement,
    ) -> Result<roundhouse_core::control::Settled, roundhouse_core::control::SpendError> {
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

/// Releases a [`GatedLedger`]'s `open_grant` gate on drop -- including on an
/// unwind from a failed assertion, which a plain statement placed after the
/// assertion would never reach.
struct ReleaseOpenGrantOnDrop<'a>(&'a GatedLedger);

impl Drop for ReleaseOpenGrantOnDrop<'_> {
    fn drop(&mut self) {
        self.0.release_open_grant();
    }
}

/// **The call's absolute expiry bounds a stalled grant too, and no request
/// is sent once it has passed.**
///
/// The grant is never released by this test: the deadline alone has to be
/// what ends the wait and parks a terminal result. Two separate claims are
/// asserted, because a fix could satisfy one and not the other — the worker
/// must *stop*, and the worker must not then buy a call whose life has
/// already run out. The counted upstream is what makes the second claim a
/// measurement rather than a hope, and the control at the end is what stops
/// `calls == 0` being vacuous.
///
/// **This does not assert `available_capacity() == 2` at the deadline**, and
/// that is the module's contract rather than a weakened test: one permit is
/// held from before the payload exists until its result is delivered or
/// swept, so a parked terminal result legitimately keeps its permit — the
/// same lifecycle `a_call_that_expires_before_it_is_sent_is_recorded_as_unfunded`
/// runs. Acknowledging the result is what must return capacity, and the
/// result's own retention clock is a different bound from the call's expiry.
#[tokio::test]
async fn a_terminal_result_parks_once_a_calls_deadline_passes_even_while_its_grant_is_stalled() {
    let (addr, calls) = counted_upstream().await;
    let ledger = GatedLedger::new();
    let runtime = runtime_with_ledger(
        addr,
        RuntimeLimits {
            call_ttl_ms: 80,
            ..limits(2)
        },
        ledger.clone(),
    );

    let (capacity, call) = fund(&runtime, "stalled").await;
    let expires_at_ms = call.expires_at_ms();
    runtime.spawn(capacity, session(), call).await;

    tokio::time::timeout(Duration::from_secs(5), ledger.open_grant_entered.notified())
        .await
        .expect("open_grant must actually be entered for this test to be about anything");

    // Released on drop -- including if the assertions below panic -- so this
    // test never leaves a worker permanently parked inside `open_grant` for
    // the rest of the process, whichever way this test ends.
    let _release_guard = ReleaseOpenGrantOnDrop(&ledger);

    // Poll for a terminal result to park, without ever calling
    // `release_open_grant` ourselves: the deadline alone, not the grant
    // finally answering, must be what produces one.
    let mut parked = None;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ready = runtime.ready(&session()).await;
        if !ready.is_empty() {
            parked = Some(ready);
            break;
        }
    }
    let Some(ready) = parked else {
        panic!(
            "no terminal result parked within 800ms of the deadline passing \
             while the grant was still stalled -- the worker is stuck inside \
             `open_grant`, with nothing about `expires_at_ms` able to reach \
             it there"
        );
    };

    assert_eq!(ready.len(), 1, "exactly one terminal result parks");
    assert!(
        ready[0].record.completed_at_ms >= expires_at_ms,
        "the terminal result must be recorded at or after the deadline, not \
         before it"
    );
    assert_eq!(
        ready[0].record.outcome,
        ClassificationOutcome::Unfunded {
            reason: FundingRefusal::Expired
        },
        "a call whose grant never answered inside its own life sent nothing, \
         so it is unfunded and expired rather than a classification or a \
         transport failure"
    );
    assert!(
        ready[0].record.outcome.spend().is_none(),
        "no socket was touched, so there is no provider spend to be unknown \
         about: {:?}",
        ready[0].record.outcome
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "and no request may be sent for a call whose life ran out while its \
         grant was still open"
    );

    // The ordinary, already-correct delivery lifecycle: once a result
    // exists, acknowledging it returns its capacity.
    drop(ready);
    runtime
        .acknowledge(&session(), &[ResponseId::new("stalled")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "capacity returns once the terminal result is acknowledged, the same \
         as any other completed call"
    );

    // The control for the absence asserted above: the same upstream, reached
    // by a second runtime whose ledger answers, counts exactly one request.
    // Without it, `calls == 0` would also pass against an upstream nothing
    // could ever reach.
    let control = runtime_with_ledger(addr, limits(2), Arc::new(MemorySpendLedger::new()));
    let (control_capacity, control_call) = fund(&control, "control").await;
    control
        .spawn(control_capacity, session(), control_call)
        .await;
    await_ready(&control, 1).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the counting upstream must actually count for the absence above to \
         be about anything"
    );
}

/// Releases a [`GatedLedger`]'s `settle_grant` gate on drop, on the same
/// unwind-safety argument as [`ReleaseOpenGrantOnDrop`].
struct ReleaseSettleOnDrop<'a>(&'a GatedLedger);

impl Drop for ReleaseSettleOnDrop<'_> {
    fn drop(&mut self) {
        self.0.release_settle();
    }
}

/// **The separate stalled-settlement case: the same required behavior as the
/// stalled-grant test above, for a call whose HTTP round trip already
/// completed and is now stuck inside `settle_grant` past its own deadline.**
///
/// The grant is released immediately so the worker reaches a real send (the
/// upstream answers with a valid, usable answer); only `settle_grant` is
/// gated, and this test never releases it. `send_and_settle` awaits the settle
/// unconditionally, with no deadline wrapped around it either -- the same
/// absence as the grant case, on the far side of a successful HTTP call rather
/// than the near side of one. As with the grant case, this does not require
/// immediate capacity release; it requires a terminal result to park within a
/// bounded time of the deadline.
///
/// **And what parks must keep everything the completed round trip already
/// established.** A bound that answered by discarding the reply would be worse
/// than the stall it replaced: the service reported 312 input and 48 output
/// tokens, this deployment's own rate card prices that at [`REPORTED_USD`], and
/// `jev-1.12` is the identity that answered. All three are facts about a round
/// trip that finished, and none of them becomes less true because a *later*
/// ledger call ran out of time. So they are asserted exactly rather than
/// loosely -- a timeout that replaced a received reply with unknown usage
/// passes a `matches!` and fails this.
///
/// The one field the abandoned settle is allowed to move is
/// [`SettlementAck`], and `Rejected` here means *unconfirmed*: this process
/// stopped waiting for an acknowledgement it never got. It is not a claim that
/// the backend rolled the settle back, and it is not a claim that the hold was
/// released -- that hold lapses on its own TTL, which is the backend's cleanup
/// window and not a second deadline this worker runs under.
#[tokio::test]
async fn a_terminal_result_parks_once_a_calls_deadline_passes_even_while_its_settlement_is_stalled()
{
    let addr = upstream(Duration::ZERO).await;
    let ledger = GatedLedger::new();
    let runtime = runtime_with_ledger(
        addr,
        RuntimeLimits {
            call_ttl_ms: 80,
            ..limits(2)
        },
        ledger.clone(),
    );
    // The grant is not what this test is about: let it through immediately.
    ledger.release_open_grant();

    let (capacity, call) = fund(&runtime, "stalled_settle").await;
    let expires_at_ms = call.expires_at_ms();
    // What the ledger was asked to hold, read off the durable intent rather
    // than recomputed here: `granted_usd` on the result must be that number.
    let requested_usd = call.intent.reservation.requested_usd;
    runtime.spawn(capacity, session(), call).await;

    tokio::time::timeout(Duration::from_secs(5), ledger.settle_entered.notified())
        .await
        .expect("settle_grant must actually be entered for this test to be about anything");

    // Released on drop -- including if the assertions below panic -- so this
    // test never leaves a worker permanently parked inside `settle_grant` for
    // the rest of the process, whichever way this test ends.
    let _release_guard = ReleaseSettleOnDrop(&ledger);

    // Poll for a terminal result to park, without ever calling
    // `release_settle` ourselves: the deadline alone, not the settle finally
    // answering, must be what produces one.
    let mut parked = None;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ready = runtime.ready(&session()).await;
        if !ready.is_empty() {
            parked = Some(ready);
            break;
        }
    }
    let Some(ready) = parked else {
        panic!(
            "no terminal result parked within 800ms of the deadline passing \
             while settlement was still stalled -- the worker is stuck \
             inside `settle_grant`, with nothing about `expires_at_ms` able \
             to reach it there"
        );
    };

    assert_eq!(ready.len(), 1, "exactly one terminal result parks");
    assert!(
        ready[0].record.completed_at_ms >= expires_at_ms,
        "the terminal result must be recorded at or after the deadline"
    );
    match &ready[0].record.outcome {
        ClassificationOutcome::Classified {
            classification,
            spend,
            reported_model,
        } => {
            assert_eq!(
                classification.intent.value,
                TurnIntent::Implement,
                "the answer the service gave survives its settle running out \
                 of time: nothing about the ledger changes what was said about \
                 this turn"
            );
            assert_eq!(
                *spend,
                EvaluationSpend::Measured {
                    usage: EvaluationUsage {
                        input_tokens: 312,
                        output_tokens: 48,
                    },
                    usd: REPORTED_USD,
                    granted_usd: requested_usd,
                    settled: SettlementAck::Unconfirmed,
                },
                "the exact usage the service reported and the exact amount \
                 this deployment's rate card prices it at both survive; only \
                 the acknowledgement is unconfirmed"
            );
            assert_eq!(
                reported_model.as_deref(),
                Some("jev-1.12"),
                "and the identity that answered is a fact about the call, not \
                 about whether its charge could be committed"
            );
        }
        other => panic!(
            "a completed round trip whose settle ran out of time is still a \
             classified answer with its accounting attached, got {other:?}"
        ),
    }

    // The ordinary, already-correct delivery lifecycle: once a result exists,
    // acknowledging it returns its capacity.
    drop(ready);
    runtime
        .acknowledge(&session(), &[ResponseId::new("stalled_settle")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "capacity returns once the terminal result is acknowledged"
    );
}

/// **The third ledger wait, inside `reserve` itself: a partial grant is handed
/// straight back through a zero-dollar settle, and that settle is awaited
/// before the refusal is returned.**
///
/// A grant of less than the quote buys nothing, so `reserve` releases it and
/// answers [`FundingRefusal::BudgetRefused`] -- but the release is a second
/// round trip to the same ledger, on a path where *no request will ever be
/// sent*. A deadline that covered only the grant and the send would leave this
/// one unbounded, and the worker would hold its admission permit forever on
/// behalf of a call that was refused before it began.
///
/// Two claims, and the counted upstream carries the second: the result must
/// park within a bounded time of the deadline, and no classifier request may be
/// made for a call the budget refused. The control at the end is what stops
/// `calls == 0` being vacuous.
///
/// **A grant timeout is not proof of no ledger side effect**, and neither is
/// this one: the zero-dollar release may well have been applied by the backend
/// after this worker stopped waiting for its acknowledgement. What
/// [`FundingRefusal::BudgetRefused`] records is the refusal and the two
/// amounts, which is what a later reader needs; the hold, if one is still open,
/// lapses on its TTL.
#[tokio::test]
async fn a_partial_grants_stalled_cleanup_settlement_still_terminates_within_the_deadline() {
    let (addr, calls) = counted_upstream().await;
    // A grant of zero against a quote that is not zero: the partial
    // reservation branch, whose cleanup settle is what this test stalls.
    let ledger = GatedLedger::granting(0.0);
    let runtime = runtime_with_ledger(
        addr,
        RuntimeLimits {
            call_ttl_ms: 80,
            ..limits(2)
        },
        ledger.clone(),
    );
    // The grant answers at once. It is the *cleanup* that stalls here.
    ledger.release_open_grant();

    let (capacity, call) = fund(&runtime, "short").await;
    let expires_at_ms = call.expires_at_ms();
    let requested_usd = call.intent.reservation.requested_usd;
    assert!(
        requested_usd > 0.0,
        "the quote must be nonzero for a zero grant to be a partial one"
    );
    runtime.spawn(capacity, session(), call).await;

    tokio::time::timeout(Duration::from_secs(5), ledger.settle_entered.notified())
        .await
        .expect(
            "the partial grant's zero-dollar cleanup settle must actually be \
             entered for this test to be about anything",
        );

    let _release_guard = ReleaseSettleOnDrop(&ledger);

    let mut parked = None;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ready = runtime.ready(&session()).await;
        if !ready.is_empty() {
            parked = Some(ready);
            break;
        }
    }
    let Some(ready) = parked else {
        panic!(
            "no terminal result parked within 800ms of the deadline passing \
             while the partial grant's cleanup settle was still stalled -- the \
             worker is stuck inside `reserve`, before any request was even \
             considered"
        );
    };

    assert_eq!(ready.len(), 1, "exactly one terminal result parks");
    assert!(
        ready[0].record.completed_at_ms >= expires_at_ms,
        "the terminal result must be recorded at or after the deadline"
    );
    assert_eq!(
        ready[0].record.outcome,
        ClassificationOutcome::Unfunded {
            reason: FundingRefusal::BudgetRefused {
                requested_usd,
                granted_usd: 0.0,
            }
        },
        "the refusal that was already established keeps both of its amounts: \
         a cleanup settle running out of time does not turn a budget refusal \
         into an expiry"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a call the budget refused sends nothing, whatever its cleanup settle \
         did"
    );

    drop(ready);
    runtime
        .acknowledge(&session(), &[ResponseId::new("short")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "capacity returns once the terminal result is acknowledged"
    );

    // The control for the absence asserted above: the same upstream, reached
    // by a second runtime whose ledger grants in full, counts exactly one
    // request.
    let control = runtime_with_ledger(addr, limits(2), Arc::new(MemorySpendLedger::new()));
    let (control_capacity, control_call) = fund(&control, "control").await;
    control
        .spawn(control_capacity, session(), control_call)
        .await;
    await_ready(&control, 1).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the counting upstream must actually count for the absence above to \
         be about anything"
    );
}

/// **Positive control: a grant that releases inside the call's life hands the
/// send only what is left of that life, not a fresh window.**
///
/// This used to hold the grant *past* the deadline and then expect the send to
/// run anyway and fail on it. Under one absolute deadline across queue, grant,
/// send and settle that state no longer reaches a send at all -- the grant wait
/// is itself cut off and the result is
/// [`FundingRefusal::Expired`], which the stalled-grant regression above is
/// what covers. So the grant releases *before* expiry here, which is the case
/// this control is actually about: 300ms of a 400ms life spent inside
/// `open_grant`, leaving the send roughly a quarter of the original budget.
///
/// The upstream's gate is never opened, so nothing but a deadline can end that
/// send. Two deadlines could: the call's own remaining ~100ms, or the
/// transport's fresh 5s window. The outer bound is 2s, so a send handed the
/// second one fails here loudly rather than passing slowly, and the explicit
/// margin below says the same thing in the record's own timestamps.
///
/// **Nothing here asserts a [`SettlementAck`]**, deliberately. Past the
/// deadline the zero-dollar settle that follows a failed send resolves or does
/// not depending on whether that ledger's future happens to complete on its
/// first poll, which is an accident of the ledger and not a contract. The
/// stalled-settlement regression above is where that field is asserted, against
/// a gate that is deterministically pending.
#[tokio::test]
async fn a_grant_released_before_expiry_leaves_the_send_only_the_time_that_remains() {
    // Never opened. The only thing that may end this send is a deadline.
    let gate = Gate::new();
    let addr = gated_upstream(vec![Arc::clone(&gate)]).await;
    let ledger = GatedLedger::new();
    let runtime = runtime_with_ledger(
        addr,
        RuntimeLimits {
            call_ttl_ms: 400,
            ..limits(2)
        },
        ledger.clone(),
    );

    let (capacity, call) = fund(&runtime, "runway").await;
    let expires_at_ms = call.expires_at_ms();
    runtime.spawn(capacity, session(), call).await;

    tokio::time::timeout(Duration::from_secs(5), ledger.open_grant_entered.notified())
        .await
        .expect("open_grant must actually be entered for this test to be about anything");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        roundhouse_core::now_ms() < expires_at_ms,
        "the grant must release inside the call's life for this test to be \
         about the send's share of what remained"
    );
    // `GatedLedger` holds `settle_grant` by default too, and the failed send
    // below releases its hold at zero; without this the worker would stall a
    // second time on a ledger wait this test is not about.
    ledger.release_open_grant();
    ledger.release_settle();

    let ready = tokio::time::timeout(Duration::from_secs(2), await_ready(&runtime, 1))
        .await
        .expect(
            "the send must be cut off by what remained of the call's own \
             deadline; a hang here means it was handed the transport's fresh \
             5s window instead",
        );
    match &ready[0].record.outcome {
        ClassificationOutcome::Failed { reason, spend } => {
            assert_eq!(
                reason, "deadline_exceeded",
                "the send honours the call's original deadline once the grant \
                 releases: {:?}",
                ready[0].record.outcome
            );
            assert!(
                matches!(spend, EvaluationSpend::Unknown { .. }),
                "nothing priceable came back, so the cost is unknown rather \
                 than measured or invented as free: {spend:?}"
            );
        }
        other => panic!(
            "expected a deadline-exceeded send once the grant released with \
             part of the call's life left, got {other:?}"
        ),
    }
    let completed_at_ms = ready[0].record.completed_at_ms;
    assert!(
        completed_at_ms >= expires_at_ms,
        "the send ran until the call's own deadline, so completion is at or \
         after it: {completed_at_ms} vs {expires_at_ms}"
    );
    assert!(
        completed_at_ms < expires_at_ms + 500,
        "and it ended *at* that deadline rather than at a fresh transport \
         window opened when the grant released: {completed_at_ms} vs \
         {expires_at_ms}"
    );

    drop(ready);
    runtime
        .acknowledge(&session(), &[ResponseId::new("runway")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "capacity is fully reclaimed once delivered and acknowledged, the \
         same as any other completed call"
    );
}

/// Opens a [`Gate`] on drop, so a failing assertion cannot strand the
/// upstream's handler -- and, with it, the worker waiting on its reply -- for
/// the rest of the process.
struct OpenGateOnDrop<'a>(&'a Gate);

impl Drop for OpenGateOnDrop<'_> {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// **Completion is stamped when the answer actually arrived.**
///
/// A record dated at dispatch would answer every later latency question about
/// this log with the wrong number, and the ordinary zero-delay upstream cannot
/// tell the two stamps apart: submission and completion are the same
/// millisecond. Here the upstream is held at an explicit gate for a known
/// interval well inside the call's life, so "when the reply came back" and
/// "when the call was queued" are 300ms apart and the assertion distinguishes
/// them.
///
/// Phase-synchronised rather than slept at: the test waits for the request to
/// genuinely reach the upstream before it starts the interval, so a slow
/// machine changes how long this takes and never what it proves.
#[tokio::test]
async fn completion_is_stamped_when_the_answer_arrived_not_when_the_call_was_queued() {
    let gate = Gate::new();
    let addr = gated_upstream(vec![Arc::clone(&gate)]).await;
    let runtime = runtime(
        addr,
        RuntimeLimits {
            call_ttl_ms: 5_000,
            ..limits(2)
        },
    );

    let (capacity, call) = fund(&runtime, "delayed").await;
    let expires_at_ms = call.expires_at_ms();
    let submitted_at_ms = roundhouse_core::now_ms();
    runtime.spawn(capacity, session(), call).await;

    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("the request must actually reach the upstream to be held there");
    let _open_guard = OpenGateOnDrop(&gate);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let released_at_ms = roundhouse_core::now_ms();
    gate.open();

    let ready = tokio::time::timeout(Duration::from_secs(5), await_ready(&runtime, 1))
        .await
        .expect("the call must resolve once the upstream answers");
    assert!(
        matches!(
            ready[0].record.outcome,
            ClassificationOutcome::Classified { .. }
        ),
        "the answer arrived inside the call's life, so this is an ordinary \
         successful classification: {:?}",
        ready[0].record.outcome
    );
    let completed_at_ms = ready[0].record.completed_at_ms;
    assert!(
        completed_at_ms >= released_at_ms,
        "completion is stamped when the reply came back, not when the call \
         was dispatched: {completed_at_ms} vs a release at {released_at_ms}"
    );
    assert!(
        completed_at_ms >= submitted_at_ms + 300,
        "and the 300ms the upstream was held for is inside that interval: \
         {completed_at_ms} vs a submission at {submitted_at_ms}"
    );
    assert!(
        completed_at_ms < expires_at_ms,
        "the call finished inside its own life, so no deadline is what this \
         test measured: {completed_at_ms} vs {expires_at_ms}"
    );
}

/// **Retention is counted from completion, and a slow call is where that
/// stops being a distinction without a difference.**
///
/// [`an_idle_session_is_reclaimed_by_the_global_sweep`] shows the separation
/// against a call that finished immediately, where a retention clock started at
/// submission would land within a millisecond of one started at completion.
/// This holds the upstream for 400ms against a 200ms retention, so the two
/// clocks are 400ms apart and a result parked under the wrong one would already
/// have lapsed before any turn could drain it -- which is the defect the
/// separate clock exists to prevent, and it is only ever visible on the slow
/// calls a bound is about in the first place.
#[tokio::test]
async fn a_slow_calls_retention_clock_starts_at_completion_not_at_submission() {
    let gate = Gate::new();
    let addr = gated_upstream(vec![Arc::clone(&gate)]).await;
    let runtime = runtime(
        addr,
        RuntimeLimits {
            call_ttl_ms: 5_000,
            result_retention_ms: 200,
            ..limits(2)
        },
    );

    let (capacity, call) = fund(&runtime, "slow").await;
    let submitted_at_ms = roundhouse_core::now_ms();
    runtime.spawn(capacity, session(), call).await;

    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("the request must actually reach the upstream to be held there");
    let _open_guard = OpenGateOnDrop(&gate);

    tokio::time::sleep(Duration::from_millis(400)).await;
    gate.open();

    let ready = tokio::time::timeout(Duration::from_secs(5), await_ready(&runtime, 1))
        .await
        .expect("the call must resolve once the upstream answers");
    let completed_at_ms = ready[0].record.completed_at_ms;
    let retain_until_ms = ready[0].retain_until_ms();
    drop(ready);

    assert!(
        retain_until_ms >= completed_at_ms + 200,
        "a finished result is held for its full retention interval, counted \
         from when it finished: {retain_until_ms} vs {completed_at_ms}"
    );
    assert!(
        retain_until_ms > submitted_at_ms + 200,
        "retention counted from submission would already have lapsed by the \
         time this answer parked: {retain_until_ms} vs a submission at \
         {submitted_at_ms}"
    );
    assert_eq!(
        runtime.sweep(completed_at_ms + 199).await,
        0,
        "nothing is reclaimed before the interval that started at completion \
         has run"
    );
    assert_eq!(
        runtime.sweep(retain_until_ms + 1).await,
        1,
        "and the result is reclaimed once it has"
    );
    assert_eq!(
        runtime.available_capacity(),
        2,
        "its permit comes back with it"
    );
}

/// The supervisor reclaims on its own clock, with nothing else running.
#[tokio::test]
async fn the_supervisor_sweeps_without_a_turn() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(
        addr,
        RuntimeLimits {
            call_ttl_ms: 5_000,
            result_retention_ms: 60,
            sweep_interval_ms: 20,
            ..limits(2)
        },
    );

    let (capacity, funded) = fund(&runtime, "call_1").await;
    runtime.spawn(capacity, session(), funded).await;
    let handles = await_ready(&runtime, 1).await;
    drop(handles);

    let _supervisor = runtime.supervise();
    for _ in 0..100 {
        if runtime.available_capacity() == 2 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the supervisor never reclaimed the expired result");
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

/// **A repair occupies admission while it runs.**
///
/// The permit is what bounds this work. Without one, a deployment whose
/// evaluation ledger is down would start a repair per unrepaired settlement per
/// turn, forever, against a ledger that is already failing — the classic way a
/// recovery path turns an outage into an outage plus a thundering herd.
///
/// **This control stops at "while it runs" deliberately.** What happens to the
/// permit once the answer parks is a separate, disputed claim — see
/// [`a_parked_repair_acknowledgement_should_retain_its_admission_permit`] — and
/// this test must keep passing whichever way that one is resolved.
#[tokio::test]
async fn a_repair_holds_admission_while_it_runs() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(1), Arc::new(MemorySpendLedger::new()));
    let capacity = runtime.capacity().expect("a free slot");
    assert_eq!(runtime.available_capacity(), 0, "the permit is taken");

    runtime
        .repair(
            capacity,
            session(),
            Principal::default_open(),
            unconfirmed(REPORTED_USD),
        )
        .await;

    let mut parked = Vec::new();
    for _ in 0..300 {
        parked = runtime.ready_repairs(&session()).await;
        if !parked.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(parked.len(), 1, "the repair produced an acknowledgement");
    assert!(
        parked[0].record.applied,
        "this ledger had never seen the call, so the repair is its first \
         application"
    );
}

/// Park one repair acknowledgement and hand back the handles a turn would
/// have. Fails the test rather than returning empty, so no caller can mistake
/// "the ledger never answered" for "the fix released the permit".
async fn park_repair(
    runtime: &Arc<ClassificationRuntime<ByteTokenizer>>,
    session_id: &SessionId,
    call_id: &str,
) -> Vec<RepairDelivery> {
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

/// **A parked repair acknowledgement retains its admission permit until
/// delivery or expiry, the same way a parked classification result does.**
///
/// `max_in_flight` is a deployment's stated ceiling on classification work
/// outstanding against this process at once, and an acknowledgement no turn has
/// committed is outstanding by the same definition a completed-but-undelivered
/// [`CompletedCall`] is — compare
/// [`a_completed_undelivered_result_still_occupies_capacity`], where the permit
/// lives inside the record for this reason.
///
/// Releasing it at park time instead left `result_retention_ms` — a time bound,
/// not a count — as the only thing deciding how much undelivered repair state
/// one process can hold. A ledger outage that ends answers many repairs at
/// once, and the sessions least likely to have a next turn to drain them are
/// exactly the idle ones.
#[tokio::test]
async fn a_parked_repair_acknowledgement_retains_its_admission_permit() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(1), Arc::new(MemorySpendLedger::new()));

    let parked = park_repair(&runtime, &session(), "eval_repair_retained").await;
    assert_eq!(parked.len(), 1);
    // Dropped first, so what follows is about the retained entry rather than
    // about the handle this test is holding.
    drop(parked);

    assert!(
        runtime.capacity().is_none(),
        "the acknowledgement is parked and undelivered, so the permit it ran \
         under must still be retained -- max_in_flight bounds outstanding \
         repair work exactly as it bounds outstanding classification results"
    );

    // The other half of the contract: delivery releases what expiry did not
    // yet need to.
    runtime
        .acknowledge_repairs(&session(), &[ResponseId::new("eval_repair_retained")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        1,
        "acknowledging the delivered repair returns its permit, the same as \
         acknowledging a delivered classification result does"
    );
}

/// **The bound is the process's, not the session's: idle sessions holding
/// acknowledgements exhaust it together.**
///
/// The per-session map made a count bound easy to state per session and useless
/// as a ceiling on this process — a deployment recovering from an outage has
/// many sessions, and the ones with no next turn hold their acknowledgements
/// longest. Two idle sessions, two permits, and the third repair is refused
/// whichever session asks for it.
#[tokio::test]
async fn parked_acknowledgements_in_idle_sessions_share_one_admission_ceiling() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(2), Arc::new(MemorySpendLedger::new()));
    let first = SessionId::new("sess_idle_one");
    let second = SessionId::new("sess_idle_two");

    drop(park_repair(&runtime, &first, "eval_idle_one").await);
    drop(park_repair(&runtime, &second, "eval_idle_two").await);

    assert!(
        runtime.capacity().is_none(),
        "two undelivered acknowledgements in two idle sessions spend both \
         permits, so a third session's repair cannot be admitted"
    );

    runtime
        .acknowledge_repairs(&first, &[ResponseId::new("eval_idle_one")])
        .await;
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and delivering one of them frees exactly one permit, whichever \
         session it belonged to"
    );
    assert_eq!(
        runtime.retained_repairs(&second).await,
        1,
        "the other session's acknowledgement is untouched by that delivery"
    );
}

/// **A repair delivery handle holds capacity after its map entry expires**, the
/// same ownership [`a_delivery_handle_holds_capacity_after_its_map_entry_is_evicted`]
/// pins for a classification result.
///
/// The turn that is midway through writing an acknowledgement is holding the
/// handle, and the sweep runs on its own clock. Releasing the permit with the
/// map entry would hand capacity back while this process still owed a durable
/// write it had not finished.
#[tokio::test]
async fn a_repair_delivery_handle_holds_capacity_after_its_map_entry_expires() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(2), Arc::new(MemorySpendLedger::new()));

    let handles = park_repair(&runtime, &session(), "eval_repair_evicted").await;
    assert_eq!(runtime.available_capacity(), 1);

    runtime.sweep(handles[0].retain_until_ms() + 1).await;
    assert_eq!(
        runtime.retained_repairs(&session()).await,
        0,
        "the entry really was evicted"
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "capacity must stay spent while a handle still owns the acknowledgement"
    );

    drop(handles);
    assert_eq!(
        runtime.available_capacity(),
        2,
        "and comes back only when the last owner lets go"
    );
    assert_eq!(
        runtime.claimed_repairs(),
        0,
        "the identity is free again too, so a later turn may drive the \
         settlement the sweep threw the answer to"
    );
}

/// **Shutdown releases every parked acknowledgement's permit and claim.**
///
/// Explicit wind-down is the one path that must leave the runtime quiet rather
/// than merely told to stop: a permit still held after it, or an identity still
/// claimed, is state nothing can now release.
#[tokio::test]
async fn shutdown_releases_parked_acknowledgements_and_their_claims() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(1), Arc::new(MemorySpendLedger::new()));

    drop(park_repair(&runtime, &session(), "eval_repair_shutdown").await);
    assert_eq!(runtime.available_capacity(), 0);
    assert_eq!(runtime.claimed_repairs(), 1);

    runtime.shutdown().await;
    assert_eq!(
        runtime.available_capacity(),
        1,
        "the acknowledgement was dropped, so its permit came back"
    );
    assert_eq!(runtime.claimed_repairs(), 0, "and so did its identity");
    assert_eq!(runtime.retained_repairs(&session()).await, 0);
}

/// **A cancelled repair frees the identity it claimed.**
///
/// Cancellation is a future being dropped where it stands — no cleanup path
/// runs, and the ledger may or may not have applied. The settlement must stay
/// unrepaired *and* drivable: a claim that survived cancellation would leave it
/// unrepaired forever, which is the one outcome worse than a redundant
/// deduplicated retry.
#[tokio::test]
async fn a_cancelled_repair_frees_the_identity_it_claimed() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(1), Arc::new(NeverAnsweringLedger));
    let capacity = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            capacity,
            session(),
            Principal::default_open(),
            unconfirmed_call("eval_repair_cancelled", REPORTED_USD),
        )
        .await;
    for _ in 0..100 {
        if runtime.claimed_repairs() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.claimed_repairs(),
        1,
        "the premise: a worker is inside the ledger holding the claim"
    );

    runtime.shutdown().await;
    assert_eq!(
        runtime.claimed_repairs(),
        0,
        "cancelling the worker released the identity with its permit"
    );
    assert_eq!(runtime.available_capacity(), 1);
    assert!(
        runtime.ready_repairs(&session()).await.is_empty(),
        "and recorded nothing: whether the ledger applied is exactly what a \
         cancelled worker does not know"
    );
}

/// **A repair whose ledger never answers still ends, on its own bound, and
/// parks nothing.**
///
/// Two claims in one, and the second is the load-bearing one. A worker parked
/// forever inside `settle_grant` would hold its admission permit forever, which
/// is the leak `call_ttl_ms` exists to close — and it must *not* park an
/// acknowledgement, because it has none: nothing was answered, so the log must
/// go on saying the settlement is unconfirmed and a later turn must drive it
/// again.
#[tokio::test]
async fn a_repair_that_is_never_answered_ends_on_its_own_bound_and_records_nothing() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(
        addr,
        RuntimeLimits {
            // Short enough to observe, and it is the *repair's* own bound
            // measured from now — the original call's absolute expiry is in the
            // past and binding against it would time every repair out before it
            // started.
            call_ttl_ms: 150,
            ..limits(1)
        },
        Arc::new(NeverAnsweringLedger),
    );
    let capacity = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            capacity,
            session(),
            Principal::default_open(),
            unconfirmed(REPORTED_USD),
        )
        .await;
    assert_eq!(runtime.available_capacity(), 0, "the repair is running");

    for _ in 0..100 {
        if runtime.available_capacity() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.available_capacity(),
        1,
        "the repair gave up at its own bound rather than parking in the ledger \
         forever holding a permit"
    );
    assert!(
        runtime.ready_repairs(&session()).await.is_empty(),
        "and it recorded nothing: no answer came back, so the settlement stays \
         unconfirmed in the log for a later turn to drive again"
    );
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

/// **A second attempt at a repair that is still running is refused, and an
/// unrelated settlement is not held up by it.**
///
/// The log says a settlement is unrepaired until an acknowledgement is
/// committed, which is later than the answer that resolves it — so without a
/// claim over the ledger round trip, every turn inside that window starts
/// another worker for the same call. The refused attempt must cost nothing: its
/// permit comes straight back, and a different call admitted in the same breath
/// runs to completion while the first is still held.
#[tokio::test]
async fn a_second_attempt_at_a_running_repair_is_refused_and_another_call_proceeds() {
    let addr = upstream(Duration::from_millis(0)).await;
    let ledger = OneCallGatedLedger::new("eval_repair_gated");
    let runtime = runtime_with_ledger(addr, limits(3), Arc::clone(&ledger) as Arc<dyn SpendLedger>);

    let first = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            first,
            session(),
            Principal::default_open(),
            unconfirmed_call("eval_repair_gated", REPORTED_USD),
        )
        .await;
    for _ in 0..300 {
        if ledger.entered() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ledger.entered(),
        1,
        "the premise: the first worker is genuinely inside the ledger"
    );

    let second = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            second,
            session(),
            Principal::default_open(),
            unconfirmed_call("eval_repair_gated", REPORTED_USD),
        )
        .await;
    assert_eq!(
        runtime.available_capacity(),
        2,
        "the duplicate's permit came straight back rather than being spent on \
         a second round trip for one settlement"
    );

    let other = park_repair(&runtime, &session(), "eval_repair_other").await;
    assert_eq!(
        other.len(),
        1,
        "a different call is not starved by the gate"
    );
    assert_eq!(
        ledger.entered(),
        1,
        "and the held identity was still only ever entered once"
    );

    ledger.release();
    for _ in 0..300 {
        if runtime.retained_repairs(&session()).await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.retained_repairs(&session()).await,
        2,
        "one acknowledgement per settlement once the gate opens, not one per \
         attempt"
    );
    assert_eq!(ledger.entered(), 1, "and still one ledger round trip");
}

/// **A repair whose ledger call failed is attempted again by a later turn.**
///
/// The claim is what stops a duplicate; it must not also stop a retry. A failed
/// settle leaves the settlement unrepaired in the log with no answer parked, so
/// the identity has to be free again the moment the worker ends — otherwise the
/// suppression that protects the ledger during an outage is also what would
/// strand every settlement the outage failed.
#[tokio::test]
async fn a_repair_whose_ledger_call_failed_can_be_attempted_again() {
    let addr = upstream(Duration::from_millis(0)).await;
    let ledger = FailsOnceLedger::new("eval_repair_retried");
    let runtime = runtime_with_ledger(addr, limits(1), Arc::clone(&ledger) as Arc<dyn SpendLedger>);

    let first = runtime.capacity().expect("the one permit");
    runtime
        .repair(
            first,
            session(),
            Principal::default_open(),
            unconfirmed_call("eval_repair_retried", REPORTED_USD),
        )
        .await;
    for _ in 0..300 {
        if runtime.available_capacity() == 1 && ledger.attempts() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(ledger.attempts(), 1, "the ledger refused the first attempt");
    assert!(
        runtime.ready_repairs(&session()).await.is_empty(),
        "a refusal parks nothing: there is no answer to acknowledge"
    );
    assert_eq!(
        runtime.claimed_repairs(),
        0,
        "and it holds no identity, so a later turn may drive this settlement \
         again"
    );

    let parked = park_repair(&runtime, &session(), "eval_repair_retried").await;
    assert_eq!(parked.len(), 1, "the retry ran and answered");
    assert_eq!(ledger.attempts(), 2, "on a second ledger round trip");
}

/// **A stopped runtime starts no repair.**
///
/// The same rule `spawn` is under, and for the same reason: a permit taken
/// before the lifetime ended and spent after it would start ledger work on
/// behalf of a runtime that has already been told to stop.
#[tokio::test]
async fn a_stopped_runtime_starts_no_repair() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(1), Arc::new(NeverAnsweringLedger));
    let capacity = runtime.capacity().expect("a free slot");
    runtime.stop();

    runtime
        .repair(
            capacity,
            session(),
            Principal::default_open(),
            unconfirmed(REPORTED_USD),
        )
        .await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runtime.available_capacity(),
        1,
        "the permit came straight back: nothing was started"
    );
    assert!(runtime.ready_repairs(&session()).await.is_empty());
}

// ------------------------------------------- acknowledgement-driven eviction

/// **A repair delivery handle holds capacity and its identity claim after
/// *acknowledgement* removes the map entry**, not only after the sweep's
/// retention expiry that
/// [`a_repair_delivery_handle_holds_capacity_after_its_map_entry_expires`]
/// exercises. `acknowledge_repairs` empties the map entry the moment a turn's
/// writer commits it; a second handle obtained before that call is a stand-in
/// for the writing turn's own copy, and it must still own the permit and the
/// claim until it, too, is dropped — the same [`RepairDelivery`] sharing rule
/// [`CompletedRepair`] documents, exercised through the other path that can
/// empty `repaired` underneath a live handle.
#[tokio::test]
async fn a_repair_delivery_handle_holds_capacity_after_acknowledgement_evicts_its_map_entry() {
    let addr = upstream(Duration::from_millis(0)).await;
    let runtime = runtime_with_ledger(addr, limits(1), Arc::new(MemorySpendLedger::new()));

    let call_id = "eval_repair_acked_handle";
    let first_handle = park_repair(&runtime, &session(), call_id).await;
    // A second handle taken before acknowledgement, standing in for the turn
    // that is about to acknowledge this call and is still holding its own
    // copy while it writes.
    let second_handle = runtime.ready_repairs(&session()).await;
    assert_eq!(second_handle.len(), 1);
    assert_eq!(
        runtime.claimed_repairs(),
        1,
        "the premise: one identity is held by the parked repair"
    );

    runtime
        .acknowledge_repairs(&session(), &[ResponseId::new(call_id)])
        .await;
    assert_eq!(
        runtime.retained_repairs(&session()).await,
        0,
        "the map entry is gone once acknowledged"
    );
    assert!(
        runtime.capacity().is_none(),
        "capacity must stay spent: `second_handle` still owns a clone of the \
         acknowledged entry, so the permit it shares with `first_handle` is \
         not free yet even though the map entry that used to hold it is gone"
    );
    assert_eq!(
        runtime.claimed_repairs(),
        1,
        "and the identity claim must stay held for the same reason — a live \
         handle still reads this settlement as answered"
    );

    drop(first_handle);
    assert!(
        runtime.capacity().is_none(),
        "one live handle — the acknowledging turn's own copy — is still \
         outstanding"
    );
    assert_eq!(runtime.claimed_repairs(), 1);

    drop(second_handle);
    assert!(
        runtime.capacity().is_some(),
        "and the permit returns only once the last owner lets go"
    );
    assert_eq!(
        runtime.claimed_repairs(),
        0,
        "the identity is free again too, so a later attempt at this \
         settlement would not be refused as a duplicate of one that finished \
         and was already committed"
    );
}

// ------------------------------------------------- identity scope (claim 3)

/// **The same original call identifier in two different sessions claims two
/// independent identities and progresses independently; a duplicate within
/// one session is the thing that is refused.**
///
/// `RepairIdentity` is `(SessionId, ResponseId)` — see [`claim_repair`] —
/// and every other repair test in this module reuses the one [`session`]
/// fixture, so none of them actually vary the session half of that pair.
/// This one does, over two distinct principals: [`MemorySpendLedger`]
/// deduplicates `SettlementKey::OncePerCall` per project (`settled_calls` on
/// `ProjectAccount`), so two sessions sharing one principal and one call id
/// would have their second settle answered `applied: false` by the ledger's
/// own dedup — indistinguishable from the runtime having refused a duplicate
/// itself. Distinct principals rule that confound out, so `applied: true`
/// on both sides is evidence of two genuinely independent ledger
/// applications, not two reads of one.
#[tokio::test]
async fn the_same_call_id_in_two_sessions_progresses_independently_of_a_same_session_duplicate() {
    let addr = upstream(Duration::from_millis(0)).await;
    let ledger = OneCallGatedLedger::new("eval_dup_call");
    let runtime = runtime_with_ledger(addr, limits(4), Arc::clone(&ledger) as Arc<dyn SpendLedger>);

    let session_a = SessionId::new("sess_dup_a");
    let session_b = SessionId::new("sess_dup_b");
    let principal_a = Principal::new("proj_dup_a", "user_dup_a");
    let principal_b = Principal::new("proj_dup_b", "user_dup_b");

    let first = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            first,
            session_a.clone(),
            principal_a.clone(),
            unconfirmed_call("eval_dup_call", REPORTED_USD),
        )
        .await;
    for _ in 0..300 {
        if ledger.entered() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ledger.entered(),
        1,
        "the premise: the first worker genuinely reached the ledger"
    );

    // Same session, same call id: a duplicate, refused without a second
    // ledger round trip.
    let duplicate = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            duplicate,
            session_a.clone(),
            principal_a.clone(),
            unconfirmed_call("eval_dup_call", REPORTED_USD),
        )
        .await;
    assert_eq!(
        ledger.entered(),
        1,
        "a duplicate within the same session must not reach the ledger a \
         second time while the first attempt is still running"
    );

    // A different session, the same call id: a different identity, so it
    // proceeds rather than being refused as a duplicate.
    let other = runtime.capacity().expect("a free slot");
    runtime
        .repair(
            other,
            session_b.clone(),
            principal_b.clone(),
            unconfirmed_call("eval_dup_call", REPORTED_USD),
        )
        .await;
    for _ in 0..300 {
        if ledger.entered() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ledger.entered(),
        2,
        "the same call id under a different session must reach the ledger \
         independently — one more actual ledger entry, not zero"
    );
    assert_eq!(
        runtime.claimed_repairs(),
        2,
        "both sessions' identities are held at once: (session_a, call) and \
         (session_b, call) are two distinct claims counted by the runtime, \
         not one identity shared across sessions"
    );

    ledger.release();
    let mut a_parked = Vec::new();
    let mut b_parked = Vec::new();
    for _ in 0..300 {
        a_parked = runtime.ready_repairs(&session_a).await;
        b_parked = runtime.ready_repairs(&session_b).await;
        if !a_parked.is_empty() && !b_parked.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a_parked.len(), 1, "session_a's repair answered");
    assert_eq!(
        b_parked.len(),
        1,
        "session_b's repair answered independently"
    );
    assert!(
        a_parked[0].record.applied,
        "session_a's settle was this ledger's first application under its \
         own project, not a dedup"
    );
    assert!(
        b_parked[0].record.applied,
        "and session_b's settle applied independently under its own \
         project — distinct principals are what keeps the ledger's own \
         SettlementKey::OncePerCall dedup from collapsing the two sessions' \
         identical call id into one settled entry"
    );
    // Both identities stay claimed while their acknowledgements sit parked
    // and undelivered -- the same rule `CompletedRepair` holds its permit
    // under, and what `a_repair_delivery_handle_holds_capacity_after_acknowledgement_evicts_its_map_entry`
    // exercises for one identity. Only a committed acknowledgement releases
    // either.
    assert_eq!(
        runtime.claimed_repairs(),
        2,
        "an answered-but-undelivered repair still reads its settlement as \
         unrepaired-in-progress, so both identities stay claimed until a \
         turn commits their acknowledgements"
    );

    drop(a_parked);
    drop(b_parked);
    runtime
        .acknowledge_repairs(&session_a, &[ResponseId::new("eval_dup_call")])
        .await;
    runtime
        .acknowledge_repairs(&session_b, &[ResponseId::new("eval_dup_call")])
        .await;
    assert_eq!(
        runtime.claimed_repairs(),
        0,
        "both identities are released once their acknowledgements are \
         committed and their handles dropped"
    );
}

// ------------------------------------------- the per-turn repair batch bound

/// A backlog of `size` distinct unconfirmed settlements, oldest first, in the
/// order a session's log folds them.
///
/// Distinct call ids throughout, so nothing here can be mistaken for the
/// same-identity suppression `repair_batch` deliberately does not do:
/// selecting a batch is about how much one turn looks at, and refusing a
/// duplicate is about who is already working on it.
fn backlog(size: usize) -> Vec<UnconfirmedSettlement> {
    (1..=size)
        .map(|i| unconfirmed_call(&format!("eval_backlog_{i}"), 0.01))
        .collect()
}

fn call_ids<'a>(batch: impl IntoIterator<Item = &'a UnconfirmedSettlement>) -> Vec<String> {
    batch
        .into_iter()
        .map(|settlement| settlement.call_id.to_string())
        .collect()
}

/// Count iterator pulls to distinguish bounded traversal from collecting the whole backlog.
struct CountingBacklog<'a> {
    entries: std::slice::Iter<'a, UnconfirmedSettlement>,
    pulled: &'a std::cell::Cell<usize>,
}

impl<'a> Iterator for CountingBacklog<'a> {
    type Item = &'a UnconfirmedSettlement;

    fn next(&mut self) -> Option<Self::Item> {
        self.pulled.set(self.pulled.get() + 1);
        self.entries.next()
    }
}

/// **One turn considers at most `max_in_flight` settlements, whatever the
/// outage left behind.**
///
/// What this proves, exactly: the candidates a turn iterates are chosen once,
/// before it schedules anything, as a fixed prefix of the log's backlog. It is
/// therefore a bound on *selection* — and because the engine's scheduling loop
/// iterates exactly this batch and nothing else, a bound on selection is a
/// bound on the attempts one turn can make.
///
/// **That is the part the admission semaphore alone does not give**, and the
/// distinction is why this is a structural test rather than a scheduling one.
/// A permit bounds what is outstanding *at an instant*; a repair that fails
/// against a down ledger parks nothing and returns its permit the moment its
/// worker ends, so under an unbounded walk the same turn's loop could take
/// that permit again and again and march the whole backlog. Counting candidates
/// here rather than timing workers is what keeps the assertion about the shape
/// of the loop instead of about how fast this box happens to fail a settle. The
/// runtime behaviour that follows from it is covered by
/// `repair_scheduling_attempts_per_turn_under_fast_ledger_failures` in
/// `tests/classification_settlement_recovery.rs`.
///
/// Two orders of magnitude apart, so a bound that was really "the backlog" or
/// "some fraction of it" could not pass both.
#[tokio::test]
async fn a_turn_considers_at_most_max_in_flight_settlements_however_large_the_backlog() {
    const MAX_IN_FLIGHT: usize = 3;

    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(MAX_IN_FLIGHT));

    for size in [20, 2_000] {
        let backlog = backlog(size);
        let batch = call_ids(runtime.repair_batch(&backlog));
        assert_eq!(
            batch.len(),
            MAX_IN_FLIGHT,
            "a backlog of {size} under a ceiling of {MAX_IN_FLIGHT} must \
             offer one turn {MAX_IN_FLIGHT} candidates and no more -- got \
             {}",
            batch.len()
        );
        assert_eq!(
            batch,
            vec![
                "eval_backlog_1".to_string(),
                "eval_backlog_2".to_string(),
                "eval_backlog_3".to_string(),
            ],
            "and it must be the oldest prefix in the log's own order, so the \
             settlement that has waited longest is the one driven first and a \
             later turn takes the next window, backlog={size}"
        );
    }
}

/// **The control that keeps the bound honest at the small end: a backlog that
/// fits is taken whole.**
///
/// A `repair_batch` that returned an empty slice, or one candidate, would
/// satisfy every "at most" assertion above perfectly and drain nothing. Both
/// sizes here are chosen against the ceiling: one strictly below it, and one
/// exactly at it — the off-by-one a `<` written for a `<=` would break.
#[tokio::test]
async fn a_backlog_within_the_ceiling_is_offered_whole_and_in_order() {
    const MAX_IN_FLIGHT: usize = 3;

    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(MAX_IN_FLIGHT));

    for size in [1, 2, MAX_IN_FLIGHT] {
        let backlog = backlog(size);
        let batch = call_ids(runtime.repair_batch(&backlog));
        assert_eq!(
            batch.len(),
            size,
            "a backlog of {size} is within the ceiling of {MAX_IN_FLIGHT}, so \
             nothing may be held back from this turn"
        );
        assert_eq!(
            batch,
            call_ids(&backlog),
            "and the log's order is the batch's order, backlog={size}"
        );
    }
}

/// The empty case, which is the one the engine short-circuits on: a session
/// with nothing unrepaired offers no candidates and starts no worker.
#[tokio::test]
async fn an_empty_backlog_offers_no_candidates() {
    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(3));

    let empty = backlog(0);
    assert!(
        call_ids(runtime.repair_batch(&empty)).is_empty(),
        "a session with nothing unrepaired has nothing to schedule"
    );
}

/// Selecting a bounded batch must not first traverse the entire source.
#[tokio::test]
async fn a_turn_pulls_no_more_candidates_than_it_takes() {
    const MAX_IN_FLIGHT: usize = 3;

    let addr = upstream(Duration::ZERO).await;
    let runtime = runtime(addr, limits(MAX_IN_FLIGHT));

    let backlog = backlog(2_000);
    let pulled = std::cell::Cell::new(0);
    let batch = call_ids(runtime.repair_batch(CountingBacklog {
        entries: backlog.iter(),
        pulled: &pulled,
    }));

    assert_eq!(
        batch.len(),
        MAX_IN_FLIGHT,
        "the ceiling still decides what the turn keeps"
    );
    assert_eq!(
        pulled.get(),
        MAX_IN_FLIGHT,
        "and the turn must reach no further into a 2,000-entry backlog than \
         the {MAX_IN_FLIGHT} candidates it keeps -- the rest cost this turn \
         nothing, which is what lets a recovering deployment carry the whole \
         outage in the fold"
    );
}
