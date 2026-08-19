// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The [`SpendLedger`] contract as executable assertions.
//!
//! Every guarantee the trait documents lives here as a test any backend must
//! pass unchanged, exactly as [`store::contract`](crate::store::contract) does
//! for the session store. The suite that judges [`MemorySpendLedger`] is the
//! suite a Redis backend runs, which is what makes "the enforcement number is
//! the same number whichever backend holds it" a checked property rather than a
//! claim — and it matters more here than there, because the two backends will be
//! written in two languages: Rust against a `HashMap`, Lua against a hash tag.
//!
//! Every test mints a fresh [`Principal`] instead of assuming an empty ledger,
//! so one shared backend instance — one real Redis — can host the whole suite
//! without cross-test interference.
//!
//! Nothing here sleeps. A TTL lapse and a month boundary are both reached by
//! supplying a later `now_ms`, which is the reason the trait takes one at all:
//! a suite that waited out a monthly window could not be run.
//!
//! The [`spend_ledger_contract_suite!`](crate::spend_ledger_contract_suite)
//! macro is the single list of these tests. A backend instantiates the whole
//! suite with one macro call, so it gets every test or none of them — there is
//! no wiring step where a test can be forgotten for one backend and silently
//! enforced only for the others.
//!
//! The fixtures below ([`fresh_principal`], [`terms`], [`assert_usd`]) are
//! `pub` for the same reason the assertions are: a backend's own adversarial
//! tests, which live outside this crate, were otherwise writing a second
//! deliberately-identical copy of each — and two copies of "dollars compare to
//! the cent" is one edit away from two different tolerances.
//!
//! [`MONTH_START_CASES`] is here for a related reason and is not a
//! [`SpendLedger`] assertion at all: it is the table of calendar boundaries the
//! two *calendar ports* answer, one in Rust and one in Lua, and it lives beside
//! the suite because the same "both backends answer one list" argument is what
//! makes it worth having.

use crate::control::budget::{Allocation, Budget, BudgetWindow, Exhaustion};
use crate::control::spend::{
    BalanceQuery, BudgetTerms, GrantRequest, LedgerState, Settlement, SpendError, SpendLedger,
};
use crate::control::{Principal, ProjectId};
use crate::ids::{ResponseId, SessionId};

/// Hold TTL used throughout the suite. Never waited out — expiry is reached by
/// supplying a later `now_ms`.
const TTL_MS: u64 = 60_000;

/// One calendar boundary, as a question both ports are asked.
///
/// `what` is carried so a failure names the case rather than only the two
/// numbers that disagreed.
#[derive(Debug, Clone, Copy)]
pub struct MonthStartCase {
    pub what: &'static str,
    pub now_ms: u64,
    pub month_start_ms: u64,
}

/// The month boundaries the Rust and Lua calendar ports must both compute.
///
/// **Two implementations of Hinnant's civil-calendar algorithm exist in this
/// repo, in two languages, and neither can call the other**: the Rust one is
/// private to [`spend`](super) precisely so a window boundary cannot be
/// composed from parts by a caller, and the Lua one runs inside a Redis script
/// sandbox. What keeps them honest is that both are executed against this one
/// list — the Rust port in `spend`'s own unit tests, the Lua port through an
/// embedded interpreter in `roundhouse-store-redis`.
///
/// The cases are the three places a hand-rolled calendar goes wrong: a year
/// rollover (the algorithm's era arithmetic), a leap February (the one day a
/// month-length table gets wrong), and a boundary instant (which of the two
/// windows it belongs to). Every timestamp was produced by `date -u -d`.
pub const MONTH_START_CASES: &[MonthStartCase] = &[
    MonthStartCase {
        what: "a mid-month instant, 2026-08-18",
        now_ms: 1_787_011_200_000,
        month_start_ms: 1_785_542_400_000,
    },
    MonthStartCase {
        what: "the boundary instant belongs to the window it opens, 2026-08-01",
        now_ms: 1_785_542_400_000,
        month_start_ms: 1_785_542_400_000,
    },
    MonthStartCase {
        what: "the last millisecond of December is still December, 2026-12-31T23:59:59.999Z",
        now_ms: 1_798_761_599_999,
        month_start_ms: 1_796_083_200_000,
    },
    MonthStartCase {
        what: "and the next millisecond rolls the year over, 2027-01-01",
        now_ms: 1_798_761_600_000,
        month_start_ms: 1_798_761_600_000,
    },
    MonthStartCase {
        what: "a leap day is in the February that has one, 2028-02-29T12:00:00Z",
        now_ms: 1_835_438_400_000,
        month_start_ms: 1_832_976_000_000,
    },
    MonthStartCase {
        what: "the last second of a leap February, 2028-02-29T23:59:59Z",
        now_ms: 1_835_481_599_000,
        month_start_ms: 1_832_976_000_000,
    },
    MonthStartCase {
        what: "the March that follows it, 2028-03-01",
        now_ms: 1_835_481_600_000,
        month_start_ms: 1_835_481_600_000,
    },
    MonthStartCase {
        what: "a February with no leap day, 2027-02-28T12:00:00Z",
        now_ms: 1_803_816_000_000,
        month_start_ms: 1_801_440_000_000,
    },
    MonthStartCase {
        what: "the epoch, where the era arithmetic's own origin sits",
        now_ms: 0,
        month_start_ms: 0,
    },
];

/// 2026-08-18T00:00:00Z. An arbitrary but real instant, so the monthly-window
/// test is reasoning about a calendar month somebody could look up.
const AUGUST_18: u64 = 1_787_011_200_000;
/// 2026-09-01T00:00:00Z: the next month boundary after [`AUGUST_18`].
const SEPTEMBER_1: u64 = 1_788_220_800_000;

/// Dollars compare to the cent, not to the bit. Every backend accumulates
/// through floating-point addition, and a Lua implementation will not round the
/// same way a Rust one does; a contract that demanded bit equality would be
/// asserting an implementation detail rather than a balance.
#[track_caller]
pub fn assert_usd(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-6,
        "{what}: expected ${expected}, got ${actual}"
    );
}

/// A membership nothing else in the suite shares — nor anything in a backend's
/// own tests, which is why this is `pub`: one shared Redis hosts both.
pub fn fresh_principal(user: &str) -> Principal {
    Principal::new(
        ProjectId::new(format!("proj_{}", uuid::Uuid::new_v4().simple())),
        user,
    )
}

/// The suite's ordinary terms: a total window, the valve armed, warning at
/// four fifths.
pub fn terms(limit_usd: f64, allocation: Allocation) -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: 0.8,
        },
        allocation,
    }
}

/// One grant request, spelled once so the tests below read as the claims they
/// are making rather than as seven-field literals.
fn request(
    principal: &Principal,
    response_id: &str,
    requested_usd: f64,
    terms: &BudgetTerms,
    now_ms: u64,
) -> GrantRequest {
    GrantRequest {
        principal: principal.clone(),
        session_id: SessionId::new(format!("sess_{}", principal.user)),
        response_id: ResponseId::new(response_id),
        requested_usd,
        ttl_ms: TTL_MS,
        terms: terms.clone(),
        now_ms,
    }
}

/// Takes the whole [`BudgetTerms`] and hands the settle only the window it
/// reads, so the tests below keep saying "under these terms" while the type
/// keeps carrying one field.
fn settlement(
    principal: &Principal,
    response_id: &str,
    seq: u64,
    actual_usd: f64,
    terms: &BudgetTerms,
    now_ms: u64,
) -> Settlement {
    Settlement {
        principal: principal.clone(),
        session_id: SessionId::new(format!("sess_{}", principal.user)),
        seq,
        response_id: ResponseId::new(response_id),
        actual_usd,
        window: terms.budget.window,
        now_ms,
    }
}

fn query(principal: &Principal, terms: &BudgetTerms, now_ms: u64) -> BalanceQuery {
    BalanceQuery {
        principal: principal.clone(),
        terms: terms.clone(),
        now_ms,
    }
}

pub async fn a_grant_never_exceeds_the_project_remaining<L: SpendLedger>(ledger: &L) {
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    let first = ledger
        .open_grant(request(&ada, "r1", 4.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(first.granted_usd, 4.0, "a request inside the limit");

    // The held four dollars are gone from the pool even though nothing has
    // settled: a hold that did not reduce the remaining balance would let every
    // concurrent turn be granted the whole budget.
    let second = ledger
        .open_grant(request(&ada, "r2", 100.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(second.granted_usd, 6.0, "a request past what is left");

    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(balance.held_usd, 10.0, "both holds");
    assert_usd(balance.project_remaining_usd, 0.0, "nothing left to hold");
}

pub async fn a_grant_never_exceeds_the_member_ceiling_even_when_the_project_has_room<
    L: SpendLedger,
>(
    ledger: &L,
) {
    // The shadowing rule, the right way round. A member cap that lifted the
    // project's would be a limit an admin believes they have and does not; a
    // project limit that ignored the member's would make the allocation
    // decorative. Both bind, the tighter wins.
    let ada = fresh_principal("ada");
    let capped = terms(100.0, Allocation::Capped { limit_usd: 5.0 });

    let grant = ledger
        .open_grant(request(&ada, "r1", 50.0, &capped, 0))
        .await
        .unwrap();
    assert_usd(
        grant.granted_usd,
        5.0,
        "the member ceiling binds while the project has $95 to spare",
    );

    let balance = ledger.balance(query(&ada, &capped, 0)).await.unwrap();
    assert_usd(
        balance.project_remaining_usd,
        95.0,
        "the project's own room",
    );
    assert_usd(
        balance.member_remaining_usd.unwrap(),
        0.0,
        "and the member's, spent",
    );

    // The control: the same project, the same request, from a pooled
    // membership. Nothing about the project changed, so anything less than the
    // full request here would mean the ceiling was the project's all along.
    let bob = Principal::new(ada.project.clone(), "bob");
    let pooled = terms(100.0, Allocation::Pooled);
    let unbounded = ledger
        .open_grant(request(&bob, "r2", 50.0, &pooled, 0))
        .await
        .unwrap();
    assert_usd(unbounded.granted_usd, 50.0, "no second ceiling to bind");
    assert!(
        ledger
            .balance(query(&bob, &pooled, 0))
            .await
            .unwrap()
            .member_remaining_usd
            .is_none(),
        "a pooled membership has no member ceiling, which is not a ceiling of zero"
    );
}

pub async fn concurrent_grants_cannot_jointly_exceed_the_limit<L: SpendLedger + Clone>(ledger: &L) {
    // **The race this whole mechanism exists to close, and it is not a
    // multi-node race.** The turn gate is per session, so two sessions under one
    // membership run concurrently in a single process; a gate that read a
    // balance, decided, and then wrote would let both through on the same
    // remaining dollar. Genuinely spawned tasks rather than a `join_all` of
    // futures on one task, because a single-threaded interleaving cannot
    // observe a torn read-modify-write.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..20u32 {
        let ledger = ledger.clone();
        let ada = ada.clone();
        let terms = terms.clone();
        tasks.spawn(async move {
            ledger
                .open_grant(GrantRequest {
                    principal: ada,
                    session_id: SessionId::new(format!("sess_{index}")),
                    response_id: ResponseId::new(format!("resp_{index}")),
                    requested_usd: 1.0,
                    ttl_ms: TTL_MS,
                    terms,
                    now_ms: 0,
                })
                .await
                .unwrap()
                .granted_usd
        });
    }
    let mut total = 0.0;
    while let Some(granted) = tasks.join_next().await {
        total += granted.unwrap();
    }

    assert_usd(
        total,
        10.0,
        "twenty concurrent dollars against a limit of ten",
    );
    assert_usd(
        ledger
            .balance(query(&ada, &terms, 0))
            .await
            .unwrap()
            .project_remaining_usd,
        0.0,
        "and the ledger agrees with the sum of what it handed out",
    );
}

pub async fn a_held_grant_is_released_once_its_ttl_lapses<L: SpendLedger>(ledger: &L) {
    // The crash story's first half: a process that dies between grant and
    // settle leaks a hold, and the leak has to heal without a sweeper — nothing
    // in this design has a cross-session index to sweep with.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    ledger
        .open_grant(request(&ada, "abandoned", 10.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(
        ledger
            .balance(query(&ada, &terms, TTL_MS - 1))
            .await
            .unwrap()
            .project_remaining_usd,
        0.0,
        "inside the TTL the hold still binds",
    );

    // **The boundary instant, pinned.** A hold placed at 0 with a TTL of
    // `TTL_MS` expires *at* `TTL_MS`, not one millisecond after: the
    // convention is that a hold survives while `now_ms` is strictly before its
    // expiry, which the memory ledger spells `expires_at_ms > now_ms` (retain)
    // and the Lua one spells `expires <= now_ms` (delete). Two languages, one
    // convention — and an off-by-one between them would only ever show up in
    // the single millisecond neither `TTL_MS - 1` nor `TTL_MS + 1` visits.
    let boundary = ledger.balance(query(&ada, &terms, TTL_MS)).await.unwrap();
    assert_usd(
        boundary.held_usd,
        0.0,
        "the hold is gone at exactly its expiry, not one millisecond later",
    );

    // Expired by the next call that looks, not by a timer.
    let after = ledger
        .balance(query(&ada, &terms, TTL_MS + 1))
        .await
        .unwrap();
    assert_usd(after.held_usd, 0.0, "the lapsed hold");
    assert_usd(after.project_remaining_usd, 10.0, "the pool, restored");
    assert_usd(
        ledger
            .open_grant(request(&ada, "next", 10.0, &terms, TTL_MS + 1))
            .await
            .unwrap()
            .granted_usd,
        10.0,
        "and a later turn can have what the dead one was holding",
    );
}

pub async fn settle_is_idempotent_by_session_and_seq<L: SpendLedger>(ledger: &L) {
    // The same rule `MetricsFold` states for itself, and for the same reason: a
    // session re-drives its own terminal events through here on every open, so a
    // settle that were not idempotent would charge a project again for every
    // turn of every session that took more than one.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);
    ledger
        .open_grant(request(&ada, "r1", 4.0, &terms, 0))
        .await
        .unwrap();

    let first = ledger
        .settle_grant(settlement(&ada, "r1", 7, 3.0, &terms, 0))
        .await
        .unwrap();
    assert!(first.applied);
    assert_usd(first.committed_usd, 3.0, "the turn's real cost");

    let replayed = ledger
        .settle_grant(settlement(&ada, "r1", 7, 3.0, &terms, 0))
        .await
        .unwrap();
    assert!(!replayed.applied, "the same (session, seq) settled twice");
    assert_usd(replayed.committed_usd, 3.0, "committed, unchanged");
    assert_usd(
        ledger
            .balance(query(&ada, &terms, 0))
            .await
            .unwrap()
            .member_committed_usd,
        3.0,
        "the member row must not double-count either",
    );
}

pub async fn settling_below_the_hold_returns_the_difference<L: SpendLedger>(ledger: &L) {
    // The ordinary case: a grant is a ceiling computed from expected output, and
    // most turns come in under it. The difference has to go back to the pool or
    // a project's effective budget shrinks by the forecasting error of every
    // turn it ever ran.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);
    ledger
        .open_grant(request(&ada, "r1", 6.0, &terms, 0))
        .await
        .unwrap();

    let settled = ledger
        .settle_grant(settlement(&ada, "r1", 1, 2.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(settled.released_usd, 4.0, "the unspent part of the hold");
    assert_usd(settled.committed_usd, 2.0, "what was really spent");

    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(
        balance.held_usd,
        0.0,
        "the hold is gone, not partially gone",
    );
    assert_usd(balance.project_remaining_usd, 8.0, "limit minus committed");
}

pub async fn settling_above_the_hold_overcommits_rather_than_capping<L: SpendLedger>(ledger: &L) {
    // The disclosed limitation of an authorization hold, kept honest. The grant
    // is computed from `expected_output_tokens`, so a reasoning-heavy turn can
    // settle above it — and a ledger that clamped the overshoot would report a
    // number the provider's invoice will not agree with. The excess lands in
    // committed spend, visibly past the limit, and the next grant sees it.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);
    ledger
        .open_grant(request(&ada, "r1", 2.0, &terms, 0))
        .await
        .unwrap();

    let settled = ledger
        .settle_grant(settlement(&ada, "r1", 1, 14.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(settled.released_usd, 0.0, "nothing left to give back");
    assert_usd(
        settled.committed_usd,
        14.0,
        "the whole realized spend, past the $10 limit",
    );

    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(
        balance.committed_usd,
        14.0,
        "the ledger visibly exceeds its limit",
    );
    assert_usd(
        balance.project_remaining_usd,
        0.0,
        "remaining floors at zero rather than going negative",
    );
    assert_eq!(balance.state, LedgerState::Exhausted);
}

pub async fn a_settle_at_or_below_the_watermark_is_a_no_op<L: SpendLedger>(ledger: &L) {
    // The other half of idempotency, and the one a naive "have I seen this
    // response id" check would miss: a replay walks the log forward from the
    // beginning, so the settles it re-drives arrive *below* the watermark rather
    // than equal to it.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    ledger
        .settle_grant(settlement(&ada, "r5", 5, 1.0, &terms, 0))
        .await
        .unwrap();
    for (seq, response) in [(2u64, "r2"), (5, "r5")] {
        let replayed = ledger
            .settle_grant(settlement(&ada, response, seq, 1.0, &terms, 0))
            .await
            .unwrap();
        assert!(
            !replayed.applied,
            "seq {seq} is at or below the watermark and must change nothing"
        );
    }
    assert_usd(
        ledger
            .balance(query(&ada, &terms, 0))
            .await
            .unwrap()
            .committed_usd,
        1.0,
        "one settle, whatever the replay re-drove",
    );

    // The control: a seq above the watermark is a new fact and does apply.
    let fresh = ledger
        .settle_grant(settlement(&ada, "r9", 9, 1.0, &terms, 0))
        .await
        .unwrap();
    assert!(fresh.applied);
    assert_usd(fresh.committed_usd, 2.0, "the watermark advanced");
}

pub async fn an_exhausted_project_grants_zero_rather_than_erroring<L: SpendLedger>(ledger: &L) {
    // Zero is an ordinary answer. Degrade-to-local is one predicate — local
    // candidates are priced at zero dollars — so a zero grant excludes frontier
    // and admits local with no branch anywhere. An error here would instead have
    // to be caught and translated into that same behavior by every caller.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);
    ledger
        .settle_grant(settlement(&ada, "r1", 1, 10.0, &terms, 0))
        .await
        .unwrap();

    let grant = ledger
        .open_grant(request(&ada, "r2", 5.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(grant.granted_usd, 0.0, "nothing left to grant");
    assert_eq!(grant.state, LedgerState::Exhausted);
    // "A ledger can never report an overflow" used to be asserted here. It is
    // now a fact about the type: `Grant::state` is a `LedgerState`, which has
    // no such variant, so the assertion could not fail and could not compile.

    // And the warn threshold, since it is the state one step before this one:
    // a fresh project past 80% of its limit is warned, not exhausted.
    let bob = fresh_principal("bob");
    ledger
        .settle_grant(settlement(&bob, "r1", 1, 8.5, &terms, 0))
        .await
        .unwrap();
    let warned = ledger
        .open_grant(request(&bob, "r2", 0.5, &terms, 0))
        .await
        .unwrap();
    assert_usd(warned.granted_usd, 0.5, "a warned budget still grants");
    assert_eq!(warned.state, LedgerState::Warned);

    // **`Exhausted` means this turn got nothing**, and the boundary case is
    // where that matters: a grant that takes the very last dollar is not
    // exhausted, because it has a dollar. The router reads `Exhausted` as
    // "ceiling zero" — it is what arms the overflow valve and what triggers a
    // refusal — so a state judged on what the grant *left behind* would open
    // the valve for a turn that could still pay, and would refuse a turn under
    // `Exhaustion::Refuse` that had money in hand.
    let cleo = fresh_principal("cleo");
    let last = ledger
        .open_grant(request(&cleo, "r1", 10.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(last.granted_usd, 10.0, "the whole budget, in one grant");
    assert_ne!(
        last.state,
        LedgerState::Exhausted,
        "a grant of the entire budget is not a grant of nothing"
    );
    // The next one is, and that is the turn the valve is for.
    let after = ledger
        .open_grant(request(&cleo, "r2", 1.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(after.granted_usd, 0.0, "now there is nothing");
    assert_eq!(after.state, LedgerState::Exhausted);
}

pub async fn a_second_grant_under_one_response_id_replaces_the_hold_rather_than_stacking<
    L: SpendLedger,
>(
    ledger: &L,
) {
    // **A turn has one hold**, and this is the guarantee the trait states that
    // no other test reaches. The engine never opens a second grant under one
    // `ResponseId` — a deduplicated retry short-circuits before `plan` — so a
    // backend that accumulated holds instead of replacing them would pass
    // every other assertion in this suite. The failure it hides is a project
    // that runs out of budget it never spent, one leaked hold at a time, and
    // it self-heals only after a TTL nobody is waiting on.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    let first = ledger
        .open_grant(request(&ada, "r1", 4.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(first.granted_usd, 4.0, "the first hold");

    let second = ledger
        .open_grant(request(&ada, "r1", 6.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(
        second.granted_usd,
        6.0,
        "the re-grant is computed against a pool its own earlier hold is not in",
    );

    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(balance.held_usd, 6.0, "one hold, at the second amount");
    assert_usd(
        balance.project_remaining_usd,
        4.0,
        "and $4 of the $10 limit is still there — stacked holds would have left $0",
    );

    // **The step the two assertions above cannot make on their own.** A hash
    // keyed by response id replaces on write in both backends, so a re-grant
    // that computed what was available *with its own earlier hold still
    // counted* would land on exactly the numbers checked above whenever the
    // two amounts happen to sum to the limit — which $4 and $6 do. Asking for
    // the whole limit is what separates the two: replaced, it is affordable;
    // merely overwritten after the fact, it is capped at what the first hold
    // left behind.
    let third = ledger
        .open_grant(request(&ada, "r1", 10.0, &terms, 0))
        .await
        .unwrap();
    assert_usd(
        third.granted_usd,
        10.0,
        "the whole limit — a hold still counted against its own replacement would cap this at $4",
    );
    let balance = ledger.balance(query(&ada, &terms, 0)).await.unwrap();
    assert_usd(balance.held_usd, 10.0, "still one hold");
    assert_usd(
        balance.project_remaining_usd,
        0.0,
        "and it is the whole pool",
    );
}

pub async fn a_non_finite_request_is_refused_through_the_trait<L: SpendLedger>(ledger: &L) {
    // Refused rather than clamped, at whichever boundary the amount enters
    // through — which is why this is a contract test and not two private ones.
    // A `NaN` loses every `<` comparison it takes part in, so a clamped `NaN`
    // request grants zero and a clamped `NaN` settle books nothing, and in
    // both cases a provider bills for a turn the ledger recorded as free.
    let ada = fresh_principal("ada");
    let terms = terms(10.0, Allocation::Pooled);

    for (what, requested_usd) in [
        ("not a number", f64::NAN),
        ("infinite", f64::INFINITY),
        ("negative", -1.0),
    ] {
        assert!(
            matches!(
                ledger
                    .open_grant(request(&ada, "r1", requested_usd, &terms, 0))
                    .await,
                Err(SpendError::InvalidAmount {
                    field: "requested_usd",
                    ..
                })
            ),
            "an amount that is {what} must be refused, not clamped"
        );
    }

    // The control: the same request with a real amount is granted, so the
    // refusals above are about the amount rather than about the request.
    assert_usd(
        ledger
            .open_grant(request(&ada, "r1", 1.0, &terms, 0))
            .await
            .unwrap()
            .granted_usd,
        1.0,
        "the control",
    );

    // A settle that could not be priced must not arrive as zero either: that
    // is the direction where a clamp becomes free frontier traffic.
    assert!(matches!(
        ledger
            .settle_grant(settlement(&ada, "r1", 1, f64::INFINITY, &terms, 0))
            .await,
        Err(SpendError::InvalidAmount {
            field: "actual_usd",
            ..
        })
    ));
    assert!(matches!(
        ledger
            .settle_grant(settlement(&ada, "r1", 1, f64::NAN, &terms, 0))
            .await,
        Err(SpendError::InvalidAmount {
            field: "actual_usd",
            ..
        })
    ));
    // And its control: a real settle at the same seq applies.
    assert!(
        ledger
            .settle_grant(settlement(&ada, "r1", 1, 0.5, &terms, 0))
            .await
            .unwrap()
            .applied,
        "the control: a refused settle left no watermark behind"
    );
}

pub async fn a_monthly_window_resets_committed_at_its_boundary<L: SpendLedger>(ledger: &L) {
    let ada = fresh_principal("ada");
    let monthly = BudgetTerms {
        budget: Budget {
            limit_usd: 10.0,
            window: BudgetWindow::Monthly,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    };

    ledger
        .settle_grant(settlement(&ada, "r1", 1, 10.0, &monthly, AUGUST_18))
        .await
        .unwrap();
    let spent = ledger
        .balance(query(&ada, &monthly, AUGUST_18))
        .await
        .unwrap();
    assert_eq!(spent.state, LedgerState::Exhausted);
    assert_usd(spent.committed_usd, 10.0, "August's spend");

    // Evaluated on access against the supplied clock: no background task ever
    // ran between these two lines.
    let september = ledger
        .balance(query(&ada, &monthly, SEPTEMBER_1))
        .await
        .unwrap();
    assert_usd(september.committed_usd, 0.0, "a new window");
    assert_usd(september.project_remaining_usd, 10.0, "and a full budget");
    assert_eq!(september.state, LedgerState::Unconstrained);
    assert_usd(
        ledger
            .open_grant(request(&ada, "r2", 10.0, &monthly, SEPTEMBER_1))
            .await
            .unwrap()
            .granted_usd,
        10.0,
        "which the next grant can draw on",
    );

    // The control, and the reason `Total` is a separate arm rather than a very
    // long month: the same spend under a total window is still spent in
    // September.
    let bob = fresh_principal("bob");
    let total = terms(10.0, Allocation::Pooled);
    ledger
        .settle_grant(settlement(&bob, "r1", 1, 10.0, &total, AUGUST_18))
        .await
        .unwrap();
    assert_eq!(
        ledger
            .balance(query(&bob, &total, SEPTEMBER_1))
            .await
            .unwrap()
            .state,
        LedgerState::Exhausted,
        "a total window has no boundary to roll over"
    );

    // A watermark is not a window fact. Clearing them at the boundary would
    // re-apply, in September, every settle August had already accounted for.
    let replayed = ledger
        .settle_grant(settlement(&ada, "r1", 1, 10.0, &monthly, SEPTEMBER_1))
        .await
        .unwrap();
    assert!(
        !replayed.applied,
        "a settle already applied in August must not be applied again in September"
    );
}

pub async fn share_allocations_summing_past_one_are_accepted_and_the_project_limit_still_binds<
    L: SpendLedger,
>(
    ledger: &L,
) {
    // An allocation is a ceiling, not a slice of a partition. Three members each
    // allowed "60%" is a configuration an admin may write — nobody may spend
    // more than 60%, and the project limit is what stops all three together.
    // Refusing it would force every share to be re-planned each time a member
    // joins.
    let ada = fresh_principal("ada");
    let project = ada.project.clone();
    let shared = terms(100.0, Allocation::Share { fraction: 0.6 });

    let first = ledger
        .open_grant(request(&ada, "r1", 100.0, &shared, 0))
        .await
        .unwrap();
    assert_usd(first.granted_usd, 60.0, "the share binds this member");

    let bob = Principal::new(project.clone(), "bob");
    let second = ledger
        .open_grant(request(&bob, "r2", 100.0, &shared, 0))
        .await
        .unwrap();
    assert_usd(
        second.granted_usd,
        40.0,
        "the second 60% share is honored only as far as the project limit reaches",
    );

    let cleo = Principal::new(project, "cleo");
    let third = ledger
        .open_grant(request(&cleo, "r3", 100.0, &shared, 0))
        .await
        .unwrap();
    assert_usd(
        third.granted_usd,
        0.0,
        "an over-subscribed project runs out where the project limit is, not where the shares sum",
    );
    assert_eq!(third.state, LedgerState::Exhausted);
}

/// Instantiate the whole conformance suite against one backend.
///
/// This macro is the single list of contract tests — the same idiom, and for
/// the same reason, as
/// [`store_contract_suite!`](crate::store_contract_suite): a backend gets the
/// entire suite in one call, so there is no per-test wiring step where one test
/// can be forgotten for one backend while the others keep enforcing it.
///
/// `$make` is evaluated inside each generated test, so every test gets a fresh
/// ledger and a backend whose construction is async passes an `.await`
/// expression. The optional `ignore = "…"` prefix stamps that reason as
/// `#[ignore]` on every generated test — how an infrastructure-gated backend
/// applies its gate suite-wide.
///
/// ```ignore
/// roundhouse_core::spend_ledger_contract_suite!(MemorySpendLedger::new());
///
/// roundhouse_core::spend_ledger_contract_suite!(
///     ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
///     connect_from_env().await
/// );
/// ```
///
/// Only usable where the `contract` module is compiled: this crate's own tests,
/// or a dependent with the `test-support` feature on its dev-dependency.
#[macro_export]
macro_rules! spend_ledger_contract_suite {
    (ignore = $reason:literal, $make:expr $(,)?) => {
        $crate::spend_ledger_contract_suite!(@list (#[ignore = $reason]) $make);
    };
    ($make:expr $(,)?) => {
        $crate::spend_ledger_contract_suite!(@list () $make);
    };
    // The single list. Both public arms land here, so gated and ungated
    // backends cannot drift apart in coverage.
    (@list $attrs:tt $make:expr) => {
        $crate::spend_ledger_contract_suite!(@tests $attrs $make;
            a_grant_never_exceeds_the_project_remaining,
            a_grant_never_exceeds_the_member_ceiling_even_when_the_project_has_room,
            concurrent_grants_cannot_jointly_exceed_the_limit,
            a_held_grant_is_released_once_its_ttl_lapses,
            settle_is_idempotent_by_session_and_seq,
            settling_below_the_hold_returns_the_difference,
            settling_above_the_hold_overcommits_rather_than_capping,
            a_settle_at_or_below_the_watermark_is_a_no_op,
            an_exhausted_project_grants_zero_rather_than_erroring,
            a_second_grant_under_one_response_id_replaces_the_hold_rather_than_stacking,
            a_non_finite_request_is_refused_through_the_trait,
            a_monthly_window_resets_committed_at_its_boundary,
            share_allocations_summing_past_one_are_accepted_and_the_project_limit_still_binds,
        );
    };
    // One test per recursion step rather than one repetition over the names:
    // the attribute group is captured at depth one, and macro_rules cannot
    // re-expand it inside a second repetition.
    (@tests ($(#[$attr:meta])*) $make:expr; $name:ident $(, $rest:ident)* $(,)?) => {
        #[tokio::test]
        $(#[$attr])*
        async fn $name() {
            let ledger = $make;
            $crate::control::spend::contract::$name(&ledger).await;
        }
        $crate::spend_ledger_contract_suite!(@tests ($(#[$attr])*) $make; $($rest),*);
    };
    (@tests ($(#[$attr:meta])*) $make:expr; ) => {};
}
