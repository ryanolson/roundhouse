// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `OncePerCall` half of the [`SpendLedger`] contract: settlement
//! identified by the call itself rather than by a session's watermark.
//!
//! Listed in [`spend_ledger_contract_suite!`](crate::spend_ledger_contract_suite)
//! from its own child module, on the [`store::contract::learning`](crate::store::contract::learning)
//! precedent beside it: a backend that runs the session-watermark half of the
//! contract runs this half too, and the split keeps `contract.rs` from
//! carrying both settlement modes' fixtures in one file.

use crate::control::Principal;
use crate::control::budget::{Allocation, Budget, BudgetWindow};
use crate::control::spend::{BudgetTerms, Settlement, SettlementKey, SpendLedger};
use crate::ids::ResponseId;

use super::{AUGUST_18, SEPTEMBER_1, assert_usd, fresh_principal, query, request, terms};

/// The same terms under a window that rolls, for the two tests that drive
/// [`AUGUST_18`] to [`SEPTEMBER_1`]: one asking what a reset clears, the other
/// what it must not.
pub(super) fn monthly_terms(limit_usd: f64) -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            window: BudgetWindow::Monthly,
            ..terms(limit_usd, Allocation::Pooled).budget
        },
        allocation: Allocation::Pooled,
    }
}

/// An evaluation call's settle: idempotent by the call's own hold, once.
///
/// The twin of [`super::settlement`] and deliberately a separate fixture rather than a
/// flag on it: the two modes differ in what identifies a repeat, so a test that
/// could flip between them by changing one argument would read as if the choice
/// were incidental. It takes no `seq` because there is nothing ordered to name.
fn call_settlement(
    principal: &Principal,
    response_id: &str,
    actual_usd: f64,
    terms: &BudgetTerms,
    now_ms: u64,
) -> Settlement {
    Settlement {
        principal: principal.clone(),
        key: SettlementKey::OncePerCall,
        response_id: ResponseId::new(response_id),
        actual_usd,
        window: terms.budget.window,
        now_ms,
    }
}

pub async fn two_background_calls_under_one_session_settle_in_either_order<L: SpendLedger>(
    ledger: &L,
) {
    // **The defect `SettlementKey` exists to make unrepresentable.** Two
    // evaluation calls are dispatched beside one turn and finish in whatever
    // order their upstreams answer. Settled through a per-session watermark,
    // the one that finished *second* but was issued *first* carries the lower
    // log position, so it is indistinguishable from a replay: the project is
    // billed for one of two calls and the loser's hold sits until its TTL
    // lapses. Under `OncePerCall` each call is its own identity and neither can
    // read as the other's replay.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    ledger
        .open_grant(request(&ada, "call_a", 2.0, &terms, 0))
        .await
        .unwrap();
    ledger
        .open_grant(request(&ada, "call_b", 3.0, &terms, 0))
        .await
        .unwrap();

    // Newest-first, the order the watermark could not survive.
    let second_issued = ledger
        .settle_grant(call_settlement(&ada, "call_b", 3.0, &terms, 0))
        .await
        .unwrap();
    assert!(second_issued.applied);
    let first_issued = ledger
        .settle_grant(call_settlement(&ada, "call_a", 1.0, &terms, 0))
        .await
        .unwrap();
    assert!(
        first_issued.applied,
        "the call that finished last is not a replay of the one that finished first"
    );
    assert_usd(
        first_issued.released_usd,
        1.0,
        "and it releases its own hold's unspent dollar",
    );

    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(balance.committed_usd, 4.0, "both calls' realized spend");
    assert_usd(balance.held_usd, 0.0, "and both holds released");
    assert_usd(
        balance.project_remaining_usd,
        6.0,
        "what neither call spent is back in the pool",
    );
}

pub async fn a_settled_call_can_never_be_settled_again<L: SpendLedger>(ledger: &L) {
    // **The guarantee `OncePerCall` is for, at its widest.** A duplicate here
    // arrives two weeks late, after the original's hold has long lapsed, after
    // a monthly window has reset committed spend to zero, and carrying a
    // *different* amount — the shape a caller would produce if a retry re-priced
    // the call from a changed event sequence. None of that makes it a new call:
    // the record keys on identity alone.
    let ada = fresh_principal("ada");
    let monthly = monthly_terms(10.0);

    ledger
        .open_grant(request(&ada, "call_a", 4.0, &monthly, AUGUST_18))
        .await
        .unwrap();
    let settled = ledger
        .settle_grant(call_settlement(&ada, "call_a", 3.0, &monthly, AUGUST_18))
        .await
        .unwrap();
    assert!(settled.applied);
    assert_usd(settled.released_usd, 1.0, "the unspent part of the hold");
    assert_usd(settled.committed_usd, 3.0, "August's spend");

    // A sibling call, granted in the new window. The duplicate below must not
    // touch it: a deduplicated settle that still released a hold would hand
    // back money belonging to whatever call is holding that id *now*.
    ledger
        .open_grant(request(&ada, "call_b", 2.0, &monthly, SEPTEMBER_1))
        .await
        .unwrap();

    let duplicate = ledger
        .settle_grant(call_settlement(&ada, "call_a", 9.0, &monthly, SEPTEMBER_1))
        .await
        .unwrap();
    assert!(
        !duplicate.applied,
        "a settled call stays settled across a delay, a hold expiry and a window reset"
    );
    assert_usd(duplicate.released_usd, 0.0, "a duplicate releases nothing");
    assert_usd(
        duplicate.committed_usd,
        0.0,
        "and charges nothing — September's spend is still zero",
    );

    let balance = ledger
        .balance(query(&ada, &monthly, SEPTEMBER_1))
        .await
        .unwrap();
    assert_usd(balance.committed_usd, 0.0, "nothing was charged twice");
    assert_usd(
        balance.held_usd,
        2.0,
        "and the sibling call's hold is exactly where it was",
    );

    // **What permanent retention costs, pinned rather than left to be
    // discovered.** Nothing consults the settled set on the grant path, so a
    // re-grant under a settled identity succeeds and takes real money out of
    // the pool — and then cannot be settled, so it lapses on its TTL. Callers
    // mint a fresh identity per attempt; this is what a caller that does not
    // gets.
    ledger
        .open_grant(request(&ada, "call_a", 1.0, &monthly, SEPTEMBER_1))
        .await
        .unwrap();
    assert!(
        !ledger
            .settle_grant(call_settlement(&ada, "call_a", 1.0, &monthly, SEPTEMBER_1))
            .await
            .unwrap()
            .applied,
        "a re-grant under a settled identity is not a second chance at settling it"
    );
    assert_usd(
        ledger
            .balance(query(&ada, &monthly, SEPTEMBER_1))
            .await
            .unwrap()
            .held_usd,
        3.0,
        "the re-granted hold is still standing, to lapse on its TTL",
    );
}

pub async fn settled_calls_are_distinguished_by_call_and_by_project<L: SpendLedger>(ledger: &L) {
    // The control on once-only settlement: it must deduplicate a *repeat* and
    // nothing else. A record that was too coarse in either direction — one flag
    // per project, or one set shared across projects — would pass the duplicate
    // test above while silently dropping every second call.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);
    for call in ["call_a", "call_b"] {
        ledger
            .open_grant(request(&ada, call, 2.0, &terms, 0))
            .await
            .unwrap();
    }

    assert!(
        ledger
            .settle_grant(call_settlement(&ada, "call_a", 1.0, &terms, 0))
            .await
            .unwrap()
            .applied
    );
    assert!(
        ledger
            .settle_grant(call_settlement(&ada, "call_b", 2.0, &terms, 0))
            .await
            .unwrap()
            .applied,
        "a different call is a different settlement"
    );
    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(balance.committed_usd, 3.0, "both calls' realized spend");
    assert_usd(balance.held_usd, 0.0, "and both holds released");

    // `fresh_principal` mints a fresh *project*, so this is the same call id
    // under a different account — which is a different call.
    let bob = fresh_principal("bob");
    assert!(
        ledger
            .settle_grant(call_settlement(&bob, "call_a", 1.0, &terms, 0))
            .await
            .unwrap()
            .applied,
        "one project's settled call must not silence another project's"
    );
    assert_usd(
        ledger
            .balance(query(&bob, &terms, 0))
            .await
            .unwrap()
            .committed_usd,
        1.0,
        "the other project's own spend",
    );
}

pub async fn the_two_settlement_modes_do_not_share_an_identity<L: SpendLedger>(ledger: &L) {
    // One ledger, two idempotency rules, and each must be blind to the other's
    // bookkeeping. A call-mode settle that advanced a session watermark would
    // silently swallow that session's next turn; a turn's settle that claimed a
    // call record would swallow an evaluation call that happened to be keyed by
    // the same string.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    assert!(
        ledger
            .settle_grant(call_settlement(&ada, "call_a", 1.0, &terms, 0))
            .await
            .unwrap()
            .applied
    );
    assert!(
        ledger
            .settle_grant(super::settlement(&ada, "turn_1", 1, 1.0, &terms, 0))
            .await
            .unwrap()
            .applied,
        "a call-mode settle must leave the session watermark where it found it"
    );
    // The same string, now as a call identity: the turn's settle above went to
    // the watermark and must have claimed nothing here.
    assert!(
        ledger
            .settle_grant(call_settlement(&ada, "turn_1", 1.0, &terms, 0))
            .await
            .unwrap()
            .applied,
        "a watermark settle must not claim the call record for its response id"
    );

    assert_usd(
        ledger
            .balance(query(&ada, &terms, 0))
            .await
            .unwrap()
            .committed_usd,
        3.0,
        "three settles, three charges",
    );
}
