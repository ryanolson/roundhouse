// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The rest of the same one deadline: completion is stamped when the answer
//! arrived and not when the call was queued, a slow call's retention clock
//! starts at completion and not at submission, and the supervisor's sweep
//! runs without a turn ever having asked for one.

use super::*;

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
