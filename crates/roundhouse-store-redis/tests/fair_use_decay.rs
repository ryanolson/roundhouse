// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M13.1: the fair-use ledger's decay, pruning, and expiry, split out of
//! `fair_use_contract.rs` as this crate's own sibling-file convention asks
//! (see `recovery.rs`'s doc comment) once that file grew past the split's
//! themes (M13.1 review F1).
//!
//! **Decay and pruning** are the two ends of one story: a running sum is
//! only as good as what ages back out of it (decay, on every read that
//! touches a window), and a scope drawn against for years cannot be let to
//! hold one bucket field forever (pruning, owned by `record_draw` itself —
//! see the comment on
//! [`a_draw_past_the_widest_window_prunes_the_bucket_fields_it_ages_out`] for
//! why the write has to be the owner). **Expiry** is the third: what happens
//! to a scope nobody draws against again, which is Redis's own `PEXPIRE`
//! rather than anything this crate sweeps.
//!
//! Shared fixtures — the connection, the term builders, and the raw-hash
//! readers both the decay and storage tests need — live in
//! `tests/common/fair_use.rs` rather than being copied here a second time.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

mod common;

use roundhouse_core::control::fair_use::{BUCKET_MS, MAX_COUNT};
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{FairUseLedger, FairUseWindow, MemoryFairUseLedger};
use roundhouse_store_redis::test_support::fair_use_scope_keys;

use common::fair_use::{
    bucket_exists, connect_fair_use_from_env, project_only, tokens_cap, window_sum,
};
use common::raw_from_env;

/// **The decay, at the boundary where exactly one bucket leaves.**
///
/// The running sum is only as good as what ages back out of it, and this is
/// the smallest step of that: two draws a bucket apart, a `now_ms` one
/// millisecond either side of the older one's departure. The sum, `from` and
/// the answer all have to move together — a ledger that decayed the answer
/// but not the stored sum would serve this turn and refuse the next one.
///
/// The memory ledger is the control on every assertion here, because it
/// re-sums its buckets from scratch and therefore cannot be wrong about which
/// ones are inside the window.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_bucket_leaving_a_window_is_subtracted_from_its_running_sum() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);
    let five_hours = FairUseWindow::FiveHours.span_ms();

    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 60, 0.0).await.unwrap();
        ledger.record_draw(&ada, BUCKET_MS, 60, 0.0).await.unwrap();
    }
    let (project_key, _) = fair_use_scope_keys(&ada);

    // A whisker before bucket 0 leaves the five-hour window: 120 tokens are
    // inside it and the turn is refused.
    let before = five_hours + BUCKET_MS - 1;
    assert_eq!(
        redis.would_exceed(&ada, &terms, before).await.unwrap(),
        memory.would_exceed(&ada, &terms, before).await.unwrap(),
    );
    assert!(
        redis
            .would_exceed(&ada, &terms, before)
            .await
            .unwrap()
            .is_some(),
        "and it is a refusal they agree on, not two Nones"
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::FiveHours).await,
        Some((120, 0, 0, 1)),
        "nothing has left the window yet, so the sum still covers both buckets"
    );

    // One millisecond later bucket 0 is out, and 60 tokens is under the cap.
    let after = five_hours + BUCKET_MS;
    assert_eq!(
        redis.would_exceed(&ada, &terms, after).await.unwrap(),
        memory.would_exceed(&ada, &terms, after).await.unwrap(),
    );
    assert_eq!(redis.would_exceed(&ada, &terms, after).await.unwrap(), None);
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::FiveHours).await,
        Some((60, 0, 1, 1)),
        "the read subtracted exactly the bucket that left and moved `from` \
         past it -- the decay is persisted, not recomputed per call"
    );

    // CONTROL, and the reason the decay is per window rather than per scope:
    // the same draws are still whole inside the seven-day window, whose sum
    // this read must not have touched.
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::SevenDays).await,
        Some((120, 0, 0, 1)),
        "a five-hour read must not age the seven-day window's sum"
    );
    // And the bucket fields are still there: only the *widest* window's decay
    // deletes, because only it knows a bucket is outside every window.
    assert!(bucket_exists(&mut raw, &project_key, 0).await);
}

/// An idle shorter than the window leaves the sum exactly where it was.
///
/// The control on every decay assertion in this file: a read that aged
/// something out when nothing had left the window would pass most of them —
/// sums only ever move down — and fail this one.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_idle_shorter_than_the_window_leaves_the_running_sum_untouched() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);

    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 100, 0.0).await.unwrap();
    }
    let (project_key, _) = fair_use_scope_keys(&ada);

    // Four hours and twenty-five minutes later: inside the five-hour window
    // by a comfortable margin, however many buckets have gone by.
    let idle = 3 * 60 * 60_000 + 17 * BUCKET_MS;
    assert_eq!(
        redis.would_exceed(&ada, &terms, idle).await.unwrap(),
        memory.would_exceed(&ada, &terms, idle).await.unwrap(),
    );
    assert!(
        redis
            .would_exceed(&ada, &terms, idle)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::FiveHours).await,
        Some((100, 0, 0, 0)),
        "no bucket has left the window, so the read has nothing to age and \
         `from` stays where the draw put it"
    );
}

/// An idle past the narrowest window drops that window's sum and keeps the
/// wider ones', which is the branch that costs no reads at all.
///
/// The sum is *deleted* rather than zeroed: `to` — the newest bucket the sum
/// covers — is older than the window, so nothing it covers can still be
/// inside, and the whole of the answer is "start again". Keeping `to` in the
/// hash is what makes that exact rather than a bet on the caller's clock
/// never stepping backwards.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_idle_past_the_narrowest_window_drops_its_sum_and_keeps_the_widest() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    let five_hours = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);
    let a_day = project_only(vec![tokens_cap(FairUseWindow::TwentyFourHours, 100)]);

    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 100, 0.0).await.unwrap();
    }
    let (project_key, _) = fair_use_scope_keys(&ada);

    // Six hours later: past the five-hour window, nowhere near the 24-hour
    // one.
    let now_ms = 6 * 60 * 60_000;
    assert_eq!(
        redis.would_exceed(&ada, &five_hours, now_ms).await.unwrap(),
        memory
            .would_exceed(&ada, &five_hours, now_ms)
            .await
            .unwrap(),
    );
    assert_eq!(
        redis.would_exceed(&ada, &five_hours, now_ms).await.unwrap(),
        None,
        "the draw has aged out of the five-hour window"
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::FiveHours).await,
        None,
        "a window whose every bucket has aged out carries no sum at all, \
         which is the same state it had before the first draw"
    );

    // CONTROL: the 24-hour window still holds the identical draw, and both
    // ledgers still refuse on it. Without this, a decay that simply deleted
    // every window's sum would pass the assertions above.
    assert_eq!(
        redis.would_exceed(&ada, &a_day, now_ms).await.unwrap(),
        memory.would_exceed(&ada, &a_day, now_ms).await.unwrap(),
    );
    assert!(
        redis
            .would_exceed(&ada, &a_day, now_ms)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::TwentyFourHours).await,
        Some((100, 0, 0, 0)),
    );
    // And the bucket the draw landed in is still there: it is outside the
    // five-hour window and inside the widest one, and only the widest one's
    // decay deletes.
    assert!(bucket_exists(&mut raw, &project_key, 0).await);
}

/// **The pruning pass, owned.** A draw that lands past the widest window
/// deletes the bucket fields that window just aged out.
///
/// This is the objection M13 raised against a hash per scope — "it needs a
/// pruning pass nothing currently owns" — answered rather than inherited. The
/// owner is `record_draw`, and it has to be: a membership capped only on the
/// five-hour window would never ask the seven-day window anything, so a
/// pruning pass that ran only on the read would never run at all for it. An
/// idle scope is still Redis's to delete, by the `PEXPIRE` asserted below;
/// this is the *busy* scope, whose hash never expires.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_draw_past_the_widest_window_prunes_the_bucket_fields_it_ages_out() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);

    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 100, 0.0).await.unwrap();
    }
    let (project_key, _) = fair_use_scope_keys(&ada);
    // CONTROL: before the second draw the old bucket is still stored.
    assert!(bucket_exists(&mut raw, &project_key, 0).await);

    // Eight days later — one day past the widest window.
    let later = 8 * 24 * 60 * 60_000;
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, later, 100, 0.0).await.unwrap();
    }
    assert!(
        !bucket_exists(&mut raw, &project_key, 0).await,
        "the widest window's decay deletes the bucket fields it ages out, so \
         a scope drawn against for years holds at most one widest window of \
         them"
    );
    assert!(
        bucket_exists(&mut raw, &project_key, later).await,
        "and it deletes only what left: the draw that did the pruning is \
         still counted"
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::SevenDays).await,
        Some((100, 0, later / BUCKET_MS, later / BUCKET_MS)),
        "the widest window's sum was restarted from the draw that outlived \
         everything before it"
    );

    // And the read agrees with the memory ledger on both instants either side
    // of the second draw's own five-hour window.
    for now_ms in [later, later + 5 * 60 * 60_000 + BUCKET_MS] {
        assert_eq!(
            redis.would_exceed(&ada, &terms, now_ms).await.unwrap(),
            memory.would_exceed(&ada, &terms, now_ms).await.unwrap(),
            "the two ledgers must agree at {now_ms} after a prune"
        );
    }
}

/// **The retry walk agrees with the memory ledger after a decay**, compared
/// as whole refusals rather than as two lists of expected values.
///
/// The walk is the one place M13.1 left reading buckets, and it now starts
/// from a *decayed* running sum rather than from a sum recomputed out of the
/// same buckets it is about to drop. Those two are only equal if the decay
/// subtracted exactly what left; a walk that started one bucket out of step
/// would name a retry time a window-width wrong while still refusing the
/// turn, which no assertion about `is_some()` can see.
///
/// **Four draws, not three, and the cap sits where two must leave before the
/// window clears** (M13.1 refute F2). A fixture where dropping the single
/// oldest bucket is always enough to clear the cap cannot tell the real walk
/// — subtract each aged-in bucket in turn, checking after every one — from a
/// formula that assumes exactly one bucket ever has to leave and stops there;
/// both land on the same answer by coincidence. Bucket 2 staying in the sum
/// after bucket 1's departure is what forces a second iteration, and the
/// literal retry time asserted below is only reachable by actually walking
/// it.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_retry_walk_agrees_with_the_memory_ledger_after_a_decay() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 110)]);
    let five_hours = FairUseWindow::FiveHours.span_ms();

    // Four draws spread across the window, each under the cap on its own.
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 60, 0.0).await.unwrap();
        ledger.record_draw(&ada, BUCKET_MS, 60, 0.0).await.unwrap();
        ledger
            .record_draw(&ada, 2 * BUCKET_MS, 60, 0.0)
            .await
            .unwrap();
        ledger
            .record_draw(&ada, 30 * BUCKET_MS, 60, 0.0)
            .await
            .unwrap();
    }

    // Past bucket 0's departure: the decay has already dropped 60 tokens, and
    // the 180 that remain are still over the 110 cap. Which bucket has to
    // leave next is the whole content of the answer — and here it takes two:
    // dropping bucket 1 alone still leaves 120, over the cap; only after
    // bucket 2 leaves too does the sum clear it.
    let now_ms = five_hours + BUCKET_MS;
    let memory_hit = memory.would_exceed(&ada, &terms, now_ms).await.unwrap();
    let redis_hit = redis.would_exceed(&ada, &terms, now_ms).await.unwrap();
    assert_eq!(memory_hit, redis_hit);
    assert_eq!(
        memory_hit.map(|hit| hit.retry_at_ms),
        Some(3 * BUCKET_MS + five_hours),
        "bucket 2 is what has to leave next, not bucket 1: dropping bucket 1 \
         alone (120 tokens) is still over the 110 cap, so a walk that stopped \
         after the first departing bucket -- or one that never read a bucket \
         at all and assumed exactly one always suffices -- would have named \
         bucket 1's departure a window-width early"
    );

    // And once it has: both ledgers serve the turn, at the instant the
    // refusal named.
    let cleared = 3 * BUCKET_MS + five_hours;
    assert_eq!(
        redis.would_exceed(&ada, &terms, cleared).await.unwrap(),
        memory.would_exceed(&ada, &terms, cleared).await.unwrap(),
    );
    assert_eq!(
        redis.would_exceed(&ada, &terms, cleared).await.unwrap(),
        None
    );
}

/// **A saturated sum is rebuilt rather than subtracted from**, which is the
/// one hazard a running sum has that a re-summing scan did not.
///
/// A sum sitting at `MAX_COUNT` has forgotten how far past it the true total
/// went. Subtracting an aged-out bucket from it would take a window that is
/// still completely full to nearly empty — the memory ledger re-sums its
/// buckets and stays at the ceiling, so the two would disagree by the whole
/// domain, and a big enough draw would walk straight through a cap by
/// *waiting* for its own oldest bucket to age out. The decay detects
/// saturation and rebuilds the sum from the window's own buckets instead.
///
/// Reachable only above 2^53, which is why it is a differential against the
/// specification rather than an assertion about a number: the two ledgers are
/// compared, so neither list of expectations can be edited to match a
/// drifting backend.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn memory_and_redis_ledgers_agree_when_a_saturated_sum_decays() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, MAX_COUNT)]);

    // Two draws at the ceiling, a bucket apart: the sum saturates, and the
    // draw still inside the window after the first ages out is *itself* at
    // the ceiling.
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, MAX_COUNT, 0.0).await.unwrap();
        ledger
            .record_draw(&ada, BUCKET_MS, MAX_COUNT, 0.0)
            .await
            .unwrap();
    }

    let now_ms = FairUseWindow::FiveHours.span_ms() + BUCKET_MS;
    let memory_hit = memory.would_exceed(&ada, &terms, now_ms).await.unwrap();
    let redis_hit = redis.would_exceed(&ada, &terms, now_ms).await.unwrap();
    assert_eq!(
        memory_hit, redis_hit,
        "bucket 0 has aged out and bucket 1 alone is at the ceiling, so both \
         ledgers must still refuse -- a sum that had been decremented by a \
         saturated bucket's worth would be at zero and serve the turn"
    );
    assert!(
        memory_hit.is_some(),
        "and it is a refusal they agree on, not two Nones"
    );
}

/// The expiry a real draw arms is the derived one: the widest window plus one
/// bucket.
///
/// No sleeping and no seam — `PTTL` answers directly. This is the half of the
/// expiry story that is about *policy*, and it is the half a shortened TTL
/// could not check. It is armed on the scope's whole hash (M13.1), which is
/// what makes an *idle* scope cost nothing; a busy scope never expires and is
/// kept trimmed by the widest window's decay instead.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_scope_is_armed_to_expire_one_bucket_past_the_widest_window() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    ledger.record_draw(&ada, 0, 1, 0.0).await.unwrap();

    let expected = FairUseWindow::SevenDays.span_ms() + BUCKET_MS;
    let (project_key, member_key) = fair_use_scope_keys(&ada);
    for key in [&project_key, &member_key] {
        let pttl: i64 = redis::cmd("PTTL")
            .arg(key)
            .query_async(&mut raw)
            .await
            .unwrap();
        // A window either side of the round trip's own cost, rather than
        // equality: the TTL starts ticking on the server the moment the script
        // runs. What is being pinned is the *policy* — one bucket past seven
        // days — not the millisecond.
        assert!(
            pttl > expected as i64 - 5_000 && pttl <= expected as i64,
            "a scope's counters must outlive the widest window by one bucket \
             width; PTTL was {pttl}, expected about {expected}"
        );
    }
}

/// A scope that has expired stops counting, watched happening.
///
/// **Why this sleeps when nothing else in the suite does.** Every other
/// time-dependent assertion is reached by supplying a later `now_ms`, which is
/// exactly what the caller-supplied clock is for. Redis key expiry is the one
/// clock this ledger does *not* own: it is the server's, and it is what makes
/// an idle scope cost nothing rather than one hash that lives forever. The
/// only way to watch a key actually leave is to wait for it — so the
/// `test-support` seam shortens the wait to a fraction of a second while the
/// production policy, asserted directly above, stays derived from the window
/// widths.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_expired_scope_leaves_the_sum_as_well_as_the_keyspace() {
    let ledger = connect_fair_use_from_env().await.with_bucket_ttl_ms(120);
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);

    ledger.record_draw(&ada, 0, 100, 0.0).await.unwrap();
    // CONTROL: while the bucket is alive the window is spent, at a `now_ms`
    // well inside the five hours. This is the assertion that makes the one
    // below about *expiry* rather than about the window rolling.
    assert!(
        ledger
            .would_exceed(&ada, &terms, 60_000)
            .await
            .unwrap()
            .is_some(),
        "the draw counts while its bucket exists"
    );

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let (project_key, _) = fair_use_scope_keys(&ada);
    let exists: bool = redis::cmd("EXISTS")
        .arg(&project_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        !exists,
        "an idle scope is Redis's to delete; nothing in this crate sweeps one"
    );

    assert_eq!(
        ledger.would_exceed(&ada, &terms, 60_000).await.unwrap(),
        None,
        "and an expired bucket is gone from the window sum too -- the same \
         instant that was refused a moment ago is now served"
    );
}
