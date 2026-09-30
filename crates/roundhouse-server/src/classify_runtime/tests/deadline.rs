// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The call's one absolute deadline: it binds the queue wait and the send as
//! one, survives a reset after queueing, and is what ends a terminal result
//! parked behind a stalled grant, a stalled settlement, or a partial grant's
//! stalled cleanup settle -- with a grant released before expiry leaving the
//! send only the time that remains.

use super::*;

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
