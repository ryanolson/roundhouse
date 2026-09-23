// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Settlement repair bounds: a repair holds admission while it runs, a parked
//! acknowledgement retains its permit, idle sessions share one ceiling, a
//! repair delivery handle outlives its map entry, shutdown releases parked
//! claims, a cancelled repair frees the identity it claimed, an unanswered
//! repair ends on its own bound, a second attempt at a running repair is
//! refused, a ledger failure can be retried, and a stopped runtime starts none.

use super::*;

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
