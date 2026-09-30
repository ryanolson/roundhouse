// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the executor holds, and for how long, over the delivery-handle map:
//! a full queue's fast fail-open, a completed-but-undelivered result's capacity,
//! a delivery handle that outlives its map entry, a result that survives an
//! unacknowledged drain, the global sweep of idle sessions, an expired-before-send
//! call, the HTTP concurrency bound, and shutdown's cancellation and capacity
//! release. Split from classify_runtime/tests.rs (server-7, PR 18 round 1) to
//! keep each claim's file under 1000 lines; the shared fixtures live in
//! tests/mod.rs, reached here through use super::*.

use super::*;

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
