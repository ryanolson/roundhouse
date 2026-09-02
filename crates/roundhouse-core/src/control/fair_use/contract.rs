// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The [`FairUseLedger`] contract as executable assertions.
//!
//! Every guarantee the trait documents lives here as a test any backend must
//! pass unchanged, exactly as [`spend::contract`](crate::control::spend::contract)
//! does for the ledger one seam over. These assertions were
//! [`MemoryFairUseLedger`](super::MemoryFairUseLedger)'s own unit tests until
//! a second backend existed to run them; moving them here is what makes "the
//! ceiling is the same ceiling whichever backend counts it" a checked property
//! rather than a claim — and, as with spend, it matters more here than for the
//! session store, because the two implementations are written in two languages:
//! Rust over a `BTreeMap`, Lua over a hash per bucket.
//!
//! **The memory ledger is the specification.** Where a backend's own
//! representation cannot reproduce an assertion here, the backend is wrong;
//! that is the whole content of running one list twice.
//!
//! Every test mints a fresh [`Principal`] rather than assuming an empty
//! ledger, so one shared backend instance — one real Redis — can host the
//! whole suite with no cross-test interference. Nothing here sleeps: a window
//! boundary is reached by supplying a later `now_ms`, which is why
//! [`FairUseLedger::record_draw`] takes `at_ms` as data in the first place.
//!
//! The [`fair_use_ledger_contract_suite!`](crate::fair_use_ledger_contract_suite)
//! macro is the single list of these tests. A backend instantiates the whole
//! suite with one macro call, so it gets every test or none of them — there is
//! no wiring step where a test can be forgotten for one backend and silently
//! enforced only for the others.

use super::{
    BUCKET_MS, FairUseLedger, FairUseLimit, FairUseQuantity, FairUseRefusal, FairUseScope,
    FairUseTerms, FairUseWindow,
};
use crate::control::Principal;

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// A membership nothing else in the suite shares.
///
/// Borrowed from the spend contract rather than copied: it mints a
/// `Principal` over a random project, which is exactly the isolation this
/// suite needs, and the comment there already says why one shared Redis makes
/// that mandatory. A second, deliberately-identical copy is one edit away from
/// two suites colliding on the same project id.
use crate::control::spend::contract::fresh_principal;

fn tokens(window: FairUseWindow, max: u64) -> FairUseLimit {
    FairUseLimit {
        window,
        max_tokens: Some(max),
        max_usd: None,
    }
}

fn project_only(limits: Vec<FairUseLimit>) -> FairUseTerms {
    FairUseTerms {
        project: limits,
        member: Vec::new(),
    }
}

async fn refused<L: FairUseLedger + ?Sized>(
    ledger: &L,
    principal: &Principal,
    terms: &FairUseTerms,
    now_ms: u64,
) -> Option<FairUseRefusal> {
    ledger.would_exceed(principal, terms, now_ms).await.unwrap()
}

/// **The claim.** A turn over the 5-hour window is refused, and the refusal
/// carries a time a client can wait until.
pub async fn a_turn_over_the_5h_window_is_refused_with_the_earliest_retry_time<L: FairUseLedger>(
    ledger: &L,
) {
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 1_000)]);

    // One draw at t=0 that lands exactly on the cap.
    ledger.record_draw(&ada, 0, 1_000, 0.0).await.unwrap();

    let hit = refused(ledger, &ada, &terms, HOUR)
        .await
        .expect("the window is spent");
    assert_eq!(hit.window, FairUseWindow::FiveHours);
    assert_eq!(hit.scope, FairUseScope::Project);
    assert_eq!(hit.quantity, FairUseQuantity::Tokens);
    // The draw sits in bucket 0, whose end is BUCKET_MS; it leaves a
    // 5-hour window BUCKET_MS + 5h after the epoch.
    assert_eq!(hit.retry_at_ms, BUCKET_MS + 5 * HOUR);

    // CONTROL: the same ledger, the same draw, one turn earlier — under the
    // cap, so nothing is refused. Without this, a `would_exceed` that
    // refused unconditionally would pass the assertion above.
    let generous = project_only(vec![tokens(FairUseWindow::FiveHours, 1_001)]);
    assert_eq!(refused(ledger, &ada, &generous, HOUR).await, None);
}

/// The window rolls: a draw that has aged past the span stops counting, and
/// the identical request that was refused is served.
pub async fn windows_roll_rather_than_reset<L: FairUseLedger>(ledger: &L) {
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 1_000)]);
    ledger.record_draw(&ada, 0, 1_000, 0.0).await.unwrap();

    // Just inside: still refused. This is the assertion a *calendar* window
    // would fail — a 5-hour window anchored to a clock boundary would have
    // reset at some fixed hour regardless of when the draw landed.
    assert!(refused(ledger, &ada, &terms, 5 * HOUR).await.is_some());

    // Past the retry time the refusal named, and the same request is
    // served. Asserting *at* the named time rather than at some later
    // round number is what makes `retry_at_ms` a number rather than a
    // gesture.
    assert_eq!(
        refused(ledger, &ada, &terms, BUCKET_MS + 5 * HOUR).await,
        None
    );
}

/// **The member ceiling binds even when the project has room**, and one call
/// moved both scopes' counters.
///
/// The project here has *no* fair-use limit at all in the first assertion, so
/// nothing about the project's counters can be what refuses the turn — which
/// is what stops this passing for the wrong reason on a ledger that merged the
/// two scopes. The tail is the other half of the same fact and the reason
/// [`FairUseLedger::record_draw`] takes one [`Principal`] rather than two
/// arguments: the project's counter moved too, from that same single call.
pub async fn the_member_window_binds_even_when_the_projects_has_room<L: FairUseLedger>(ledger: &L) {
    let ada = fresh_principal("ada");
    let bob = Principal::new(ada.project.clone(), "bob");
    let terms = FairUseTerms {
        project: vec![tokens(FairUseWindow::FiveHours, 1_000_000)],
        member: vec![tokens(FairUseWindow::FiveHours, 100)],
    };

    ledger.record_draw(&ada, 0, 100, 0.0).await.unwrap();

    let hit = refused(ledger, &ada, &terms, HOUR)
        .await
        .expect("ada is over her own ceiling");
    assert_eq!(hit.scope, FairUseScope::Member);
    assert_eq!(
        hit.window,
        FairUseWindow::FiveHours,
        "and the refusal names the member's window, because raising the \
         project's would change nothing"
    );

    // CONTROL: the other member of the same project, under the identical
    // terms, at the identical instant. `bob` has drawn nothing, so he is
    // served — which is what makes the refusal above about `ada`'s own
    // counters rather than about the project's.
    assert_eq!(refused(ledger, &bob, &terms, HOUR).await, None);

    // And the project's own counters really did move: `ada`'s draw is in
    // the project total too, so the two scopes are two counters over one
    // draw rather than one counter read twice.
    let tight_project = project_only(vec![tokens(FairUseWindow::FiveHours, 100)]);
    assert_eq!(
        refused(ledger, &bob, &tight_project, HOUR)
            .await
            .map(|refusal| refusal.scope),
        Some(FairUseScope::Project),
        "bob has drawn nothing of his own and is still refused by the \
         project's window, which ada filled"
    );
}

/// **The narrowest window is checked first and is what the refusal names.**
///
/// Every window is over its cap here, so the answer is entirely about
/// order. Naming the 7-day one would send an agent away for a week when the
/// 5-hour one clears first.
pub async fn the_smallest_window_is_checked_first_and_named_in_the_refusal<L: FairUseLedger>(
    ledger: &L,
) {
    let ada = fresh_principal("ada");
    let terms = project_only(vec![
        tokens(FairUseWindow::SevenDays, 10),
        tokens(FairUseWindow::TwentyFourHours, 10),
        tokens(FairUseWindow::FiveHours, 10),
    ]);
    ledger.record_draw(&ada, 0, 50, 0.0).await.unwrap();

    let hit = refused(ledger, &ada, &terms, HOUR)
        .await
        .expect("every window is spent");
    assert_eq!(hit.window, FairUseWindow::FiveHours);
    assert!(hit.retry_at_ms < BUCKET_MS + 24 * HOUR);

    // CONTROL: the 5-hour window rolled off, so the next-narrowest is what
    // answers. Without this, a `would_exceed` that always returned the
    // first element of a hard-coded list would satisfy the assertion above.
    let hit = refused(ledger, &ada, &terms, BUCKET_MS + 6 * HOUR)
        .await
        .expect("the wider windows are still spent");
    assert_eq!(hit.window, FairUseWindow::TwentyFourHours);
}

/// A dollar cap and a token cap on one window both bind.
pub async fn either_cap_can_be_the_one_that_refuses<L: FairUseLedger>(ledger: &L) {
    let ada = fresh_principal("ada");
    let terms = project_only(vec![FairUseLimit {
        window: FairUseWindow::FiveHours,
        max_tokens: Some(1_000_000),
        max_usd: Some(5.0),
    }]);
    ledger.record_draw(&ada, 0, 10, 5.0).await.unwrap();

    assert_eq!(
        refused(ledger, &ada, &terms, HOUR)
            .await
            .map(|refusal| refusal.quantity),
        Some(FairUseQuantity::Usd),
        "ten tokens is nowhere near the token cap; the dollars are what ran out"
    );
}

/// A membership with no fair-use block reaches no counter and is never
/// refused — the shipped posture, and the one every project has until an
/// operator writes a window down.
pub async fn a_membership_with_no_windows_is_never_refused<L: FairUseLedger>(ledger: &L) {
    // A trillion tokens and a million dollars, not `u64::MAX` and `f64::MAX`.
    // The claim under test is "a draw of any plausible size reaches no
    // ceiling", and a saturating-add backend and a backend counting in Redis
    // integers disagree only past `i64::MAX` — a range question, answered
    // where each backend documents its own range, not smuggled into the one
    // assertion about *windows*. A number both can represent exactly is what
    // keeps this test about what its name says.
    ledger
        .record_draw(&fresh_principal("ada"), 0, 1_000_000_000_000, 1_000_000.0)
        .await
        .unwrap();
    assert_eq!(
        refused(
            ledger,
            &fresh_principal("ada"),
            &FairUseTerms::default(),
            HOUR
        )
        .await,
        None
    );
}

/// A `NaN` cannot enter the counters.
///
/// It would not blow up: it would make every window sum `NaN`, which is
/// never `>=` any cap, and the ceiling would silently stop existing — the
/// same fail-open `SpendError::check_amount` exists to prevent one seam
/// over.
pub async fn a_nonfinite_draw_is_refused_rather_than_silently_disabling_the_cap<
    L: FairUseLedger,
>(
    ledger: &L,
) {
    let ada = fresh_principal("ada");
    assert!(ledger.record_draw(&ada, 0, 1, f64::NAN).await.is_err());
    assert!(ledger.record_draw(&ada, 0, 1, -1.0).await.is_err());

    // CONTROL, and it is the load-bearing half: the counters are untouched,
    // so a refused draw is refused rather than half-applied.
    let terms = project_only(vec![FairUseLimit {
        window: FairUseWindow::FiveHours,
        max_tokens: None,
        max_usd: Some(0.01),
    }]);
    assert_eq!(refused(ledger, &ada, &terms, HOUR).await, None);
}

/// The staircase error is bounded and one-sided: early, never late.
///
/// This is also the bucket-boundary assertion, which is why it is in the
/// contract rather than beside the `BTreeMap`: a backend that built its bucket
/// index differently — rounding rather than flooring, or excluding the
/// partially-overlapping trailing bucket — passes every other test here and
/// fails this one.
pub async fn a_window_refuses_early_rather_than_late<L: FairUseLedger>(ledger: &L) {
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 10)]);
    // A draw at the very start of a bucket. Its bucket is included until
    // the bucket's *end* leaves the window, so it counts for up to one
    // bucket width longer than a per-draw ledger would say.
    ledger.record_draw(&ada, 0, 10, 0.0).await.unwrap();

    assert!(
        refused(ledger, &ada, &terms, 5 * HOUR + BUCKET_MS - 1)
            .await
            .is_some(),
        "still counted a whisker before the bucket ages out -- early"
    );
    assert_eq!(
        refused(ledger, &ada, &terms, 5 * HOUR + BUCKET_MS).await,
        None,
        "and never later than one bucket width past the span"
    );
}

/// A draw that lands in a *later* bucket than an earlier one ages out on its
/// own schedule, and the retry time names the bucket that actually has to
/// leave.
///
/// The walk in `earliest_retry_ms` is the only part of the arithmetic with more
/// than one bucket in play, and every test above records a single draw — so a
/// backend that answered the retry time from the *newest* bucket, or from the
/// window's own start, is green everywhere else and red only here.
pub async fn the_retry_time_names_the_oldest_bucket_that_has_to_age_out<L: FairUseLedger>(
    ledger: &L,
) {
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens(FairUseWindow::FiveHours, 100)]);

    // Sixty tokens in bucket 0, sixty more two buckets later. Either draw
    // alone is under the cap; together they are over it, so what has to leave
    // is exactly the first one.
    ledger.record_draw(&ada, 0, 60, 0.0).await.unwrap();
    ledger
        .record_draw(&ada, 2 * BUCKET_MS, 60, 0.0)
        .await
        .unwrap();

    let hit = refused(ledger, &ada, &terms, HOUR)
        .await
        .expect("120 tokens is over a cap of 100");
    assert_eq!(
        hit.retry_at_ms,
        BUCKET_MS + 5 * HOUR,
        "dropping bucket 0 leaves 60 under the cap, so the answer is when \
         bucket 0's end leaves the window -- not when bucket 2's does"
    );

    // CONTROL: at that named instant the window really does have room, and
    // one bucket width earlier it does not. Without the pair, a retry time
    // computed from the wrong bucket could still satisfy the equality above
    // if the constants happened to line up.
    assert!(
        refused(ledger, &ada, &terms, BUCKET_MS + 5 * HOUR - 1)
            .await
            .is_some()
    );
    assert_eq!(
        refused(ledger, &ada, &terms, BUCKET_MS + 5 * HOUR).await,
        None
    );
}

/// Instantiate the whole conformance suite against one backend.
///
/// The single list of contract tests, in the same idiom and for the same
/// reason as
/// [`spend_ledger_contract_suite!`](crate::spend_ledger_contract_suite): a
/// backend gets the entire suite in one call, so there is no per-test wiring
/// step where one test can be forgotten for one backend while the others keep
/// enforcing it.
///
/// `$make` is evaluated inside each generated test, so every test gets a fresh
/// ledger and a backend whose construction is async passes an `.await`
/// expression. The optional `ignore = "…"` prefix stamps that reason as
/// `#[ignore]` on every generated test — how an infrastructure-gated backend
/// applies its gate suite-wide.
///
/// ```ignore
/// roundhouse_core::fair_use_ledger_contract_suite!(MemoryFairUseLedger::new());
///
/// roundhouse_core::fair_use_ledger_contract_suite!(
///     ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
///     connect_from_env().await
/// );
/// ```
///
/// Only usable where the `contract` module is compiled: this crate's own tests,
/// or a dependent with the `test-support` feature on its dev-dependency.
#[macro_export]
macro_rules! fair_use_ledger_contract_suite {
    (ignore = $reason:literal, $make:expr $(,)?) => {
        $crate::fair_use_ledger_contract_suite!(@list (#[ignore = $reason]) $make);
    };
    ($make:expr $(,)?) => {
        $crate::fair_use_ledger_contract_suite!(@list () $make);
    };
    // The single list. Both public arms land here, so gated and ungated
    // backends cannot drift apart in coverage.
    (@list $attrs:tt $make:expr) => {
        $crate::fair_use_ledger_contract_suite!(@tests $attrs $make;
            a_turn_over_the_5h_window_is_refused_with_the_earliest_retry_time,
            windows_roll_rather_than_reset,
            the_member_window_binds_even_when_the_projects_has_room,
            the_smallest_window_is_checked_first_and_named_in_the_refusal,
            either_cap_can_be_the_one_that_refuses,
            a_membership_with_no_windows_is_never_refused,
            a_nonfinite_draw_is_refused_rather_than_silently_disabling_the_cap,
            a_window_refuses_early_rather_than_late,
            the_retry_time_names_the_oldest_bucket_that_has_to_age_out,
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
            $crate::control::fair_use::contract::$name(&ledger).await;
        }
        $crate::fair_use_ledger_contract_suite!(@tests ($(#[$attr])*) $make; $($rest),*);
    };
    (@tests ($(#[$attr:meta])*) $make:expr; ) => {};
}
