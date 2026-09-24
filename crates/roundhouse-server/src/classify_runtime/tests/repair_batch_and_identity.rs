// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! An acknowledgement's own eviction still leaves the delivery handle holding
//! capacity, the same call identity in two sessions progresses independently
//! of a same-session duplicate, and the per-turn repair batch bound: one turn
//! considers at most max_in_flight settlements however large the backlog,
//! offered whole and in order, empty or otherwise.

use super::*;

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
