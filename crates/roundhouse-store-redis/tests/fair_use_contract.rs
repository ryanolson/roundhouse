// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M13: the full [`FairUseLedger`] contract against a real Redis, plus the
//! adversarial cases only a real backend can exercise.
//!
//! The macro invocation is the milestone's headline, in the same idiom as
//! `contract.rs` and `spend_contract.rs`: the *same* assertions that judge
//! `MemoryFairUseLedger` judge this ledger, whatever the list grows to — the
//! macro is the list, so a test added there is added here with no wiring step.
//! The key-layout assertions need no live Redis (key strings are pure
//! formatting) and so live as unit tests beside the functions that build them,
//! rather than as ignore-gated duplicates that would depend on infrastructure
//! they do not use.
//!
//! **This file holds the unlock condition and the differentials** — the
//! claims that are about *sharing*, or about the two backends compared *side
//! by side*, rather than about decay, pruning, expiry, or raw storage. Those
//! themed slices live in this crate's own sibling-file convention (see
//! `recovery.rs`'s doc comment) instead of growing this file past it (M13.1
//! review F1): the decay/prune/expiry tests are in `fair_use_decay.rs`, and
//! the raw-storage and commandstats-loop tests are in `fair_use_storage.rs`,
//! with the fixtures both share (and this file needs too) in
//! `tests/common/fair_use.rs`.
//!
//! Below the suite is what stays here:
//!
//! - **the unlock condition itself** — two handles over one Redis, a draw
//!   through one refused by the other, which is the whole reason this ledger
//!   exists;
//! - **the differentials**: the same draws through both ledgers at the
//!   boundaries where two arithmetics used to part company — a dollar cap met
//!   exactly, and the ceiling of the shared integer domain. The contract
//!   asserts each backend against the specification; these assert the two
//!   against *each other*, which is what a list of expected values cannot do
//!   once both lists can be edited. Since M13.1's review that includes the
//!   three places a clock that does not run forwards used to part them (F6,
//!   F8, F9), all three now closed by the mark;
//! - **a window group past the ones `FairUseWindow::ALL` names**, invoked
//!   against the real script text through a `test-support` seam, because
//!   `WouldExceedArgs` deliberately cannot express one.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

mod common;

use roundhouse_core::control::fair_use::{BUCKET_MS, MAX_COUNT};
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{
    FairUseLedger, FairUseQuantity, FairUseScope, FairUseWindow, MemoryFairUseLedger,
};
use roundhouse_store_redis::test_support::{fair_use_scope_keys, fair_use_would_exceed_source};

use common::fair_use::{
    bucket_exists, connect_fair_use_from_env, project_only, tokens_cap, usd_cap, window_sum,
};
use common::raw_from_env;

roundhouse_core::fair_use_ledger_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_fair_use_from_env().await
);

/// **The unlock condition, as a test.**
///
/// `roundhouse_core::control::fair_use` deferred this implementation with one
/// sentence: *fair use across nodes is only true with shared buckets, so the
/// Redis implementation is wanted the moment a second node serves the same
/// project.* Two independently-connected ledger handles over one Redis are that
/// second node — the same thing the memory ledger cannot be, since two
/// `MemoryFairUseLedger`s share nothing at all. A draw recorded through one and
/// refused by the other is the property the whole milestone is for.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_draw_through_one_node_is_refused_by_another() {
    let node_a = connect_fair_use_from_env().await;
    let node_b = connect_fair_use_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 1_000)]);

    // CONTROL first, and it matters: before the draw, the second handle serves
    // the turn. Without it, a `would_exceed` that refused everything would
    // satisfy the assertion below while proving nothing about sharing.
    assert_eq!(
        node_b.would_exceed(&ada, &terms, 60_000).await.unwrap(),
        None,
        "nothing drawn yet, so neither node has a reason to refuse"
    );

    node_a.record_draw(&ada, 0, 1_000, 0.0).await.unwrap();

    let hit = node_b
        .would_exceed(&ada, &terms, 60_000)
        .await
        .expect("the second handle must reach the same buckets")
        .expect("a draw made through the first node fills the shared window");
    assert_eq!(hit.window, FairUseWindow::FiveHours);
    assert_eq!(hit.scope, FairUseScope::Project);
    assert_eq!(
        hit.retry_at_ms,
        BUCKET_MS + 5 * 60 * 60_000,
        "and the second node computes the same retry time from the same \
         buckets, rather than one of its own"
    );
}

/// M13 thermo-nuclear review finding F3, closed: the two ledgers agree at a
/// dollar cap boundary, because there is only one arithmetic left.
///
/// The finding was that the memory ledger accumulated `f64` while this backend
/// summed exact micro-dollars, so at an ordinary decimal boundary one admitted
/// what the other refused. $0.70 + $0.10 against a $0.80 cap is the sharpest
/// case (`0.7_f64 + 0.1_f64 == 0.7999999999999999`, strictly under), and
/// $0.10 + $0.25 against a $0.25 cap is the sharpest case for the *retry
/// walk*: dropping the first bucket leaves `0.24999999999999997` in floats and
/// exactly the cap in integers, which is a retry time two hours apart on
/// numbers that look identical.
///
/// The shared contract now asserts both against each backend on its own.
/// This is the differential the contract cannot be: the same draws through
/// both ledgers, compared as whole refusals rather than as two lists of
/// expected values that could each be updated to match a drifting backend.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn memory_and_redis_ledgers_agree_at_a_dollar_cap_boundary() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;

    let ada = fresh_principal("ada");
    let terms = project_only(vec![usd_cap(FairUseWindow::FiveHours, 0.80)]);
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 0, 0.70).await.unwrap();
        ledger.record_draw(&ada, 0, 0, 0.10).await.unwrap();
    }
    assert_eq!(
        memory.would_exceed(&ada, &terms, 60_000).await.unwrap(),
        redis.would_exceed(&ada, &terms, 60_000).await.unwrap(),
        "700000 + 100000 micro-dollars meets a cap of 800000 in both ledgers"
    );
    assert!(
        memory
            .would_exceed(&ada, &terms, 60_000)
            .await
            .unwrap()
            .is_some(),
        "and it is a refusal they agree on, not two Nones"
    );

    // The retry walk, where agreeing on `is_some()` is not enough: the two
    // answers are both refusals and differ only in the bucket they name.
    let bob = fresh_principal("bob");
    let tight = project_only(vec![usd_cap(FairUseWindow::FiveHours, 0.25)]);
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&bob, 0, 0, 0.10).await.unwrap();
        ledger.record_draw(&bob, BUCKET_MS, 0, 0.25).await.unwrap();
    }
    let now_ms = 2 * BUCKET_MS;
    let memory_hit = memory.would_exceed(&bob, &tight, now_ms).await.unwrap();
    let redis_hit = redis.would_exceed(&bob, &tight, now_ms).await.unwrap();
    assert_eq!(
        memory_hit, redis_hit,
        "dropping bucket 0 leaves exactly the cap drawn, so both must name \
         bucket 1's departure"
    );
    assert_eq!(
        memory_hit.map(|hit| hit.retry_at_ms),
        Some(2 * BUCKET_MS + 5 * 60 * 60_000),
    );
}

/// M13 thermo-nuclear review finding F7, closed: the two ledgers agree across
/// the whole domain, including at its ceiling.
///
/// The finding was that draws crossed as decimal `String`s for a documented
/// reason that did not hold — the `redis` crate encodes an integer `ARGV` as
/// the same bytes, asserted without a server in this crate's unit tests —
/// while `WOULD_EXCEED` read every bucket back through `tonumber` and summed
/// as doubles, so exactness above 2^53 was lost at the one comparison that
/// decides a refusal. Draws of 2^53+1 and 2^53+2 tokens were refused by the
/// memory ledger and served by this one.
///
/// What closed it is the bound: 2^53 is now the domain both ledgers count in,
/// a draw past it is refused by both before anything is written, and a sum at
/// it saturates in both. This is the differential over that boundary — the
/// same draws through both ledgers, compared as whole refusals.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn memory_and_redis_ledgers_agree_at_the_domain_ceiling() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let ada = fresh_principal("ada");

    // Past the domain: refused by both, the draws the finding used (2^53+1)
    // among them.
    for tokens in [MAX_COUNT + 1, MAX_COUNT + 2] {
        for ledger in [&memory as &dyn FairUseLedger, &redis] {
            assert!(
                ledger.record_draw(&ada, 0, tokens, 0.0).await.is_err(),
                "{tokens} is outside the domain both ledgers count in"
            );
        }
    }

    // Two draws at the ceiling, in two buckets so the retry walk has more
    // than one bucket to drop -- which is where a saturated sum and a
    // saturating subtraction could still disagree if only the sum were
    // clamped.
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, MAX_COUNT, 0.0).await.unwrap();
        ledger
            .record_draw(&ada, BUCKET_MS, MAX_COUNT, 0.0)
            .await
            .unwrap();
    }
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, MAX_COUNT)]);
    let memory_hit = memory
        .would_exceed(&ada, &terms, 2 * BUCKET_MS)
        .await
        .unwrap();
    let redis_hit = redis
        .would_exceed(&ada, &terms, 2 * BUCKET_MS)
        .await
        .unwrap();
    assert_eq!(
        memory_hit, redis_hit,
        "a saturated window sum and the walk that drops buckets out of it are \
         one arithmetic, so both ledgers must name the same bucket"
    );
    assert_eq!(
        memory_hit.map(|hit| hit.retry_at_ms),
        Some(BUCKET_MS + 5 * 60 * 60_000),
        "dropping bucket 0 takes the saturated sum to zero, so bucket 0 is \
         what has to leave"
    );
}

/// Appends one 8-`ARGV` window group in `WOULD_EXCEED`'s own layout: span_ms,
/// name, then project's (flag, token cap, usd cap) followed by member's.
#[allow(clippy::too_many_arguments)]
fn push_window(
    invocation: &mut redis::ScriptInvocation<'_>,
    span_ms: u64,
    name: &str,
    project_present: bool,
    project_max_tokens: Option<u64>,
    member_present: bool,
    member_max_tokens: Option<u64>,
) {
    invocation.arg(span_ms).arg(name);
    push_scope(invocation, project_present, project_max_tokens);
    push_scope(invocation, member_present, member_max_tokens);
}

fn push_scope(
    invocation: &mut redis::ScriptInvocation<'_>,
    present: bool,
    max_tokens: Option<u64>,
) {
    invocation.arg(if present { "1" } else { "0" });
    invocation.arg(max_tokens.map(|t| t.to_string()).unwrap_or_default());
    // No dollar cap in any group this test builds -- the sentinel empty
    // string, same as `ScopeCaps::absent`'s `max_micros`.
    invocation.arg("");
}

/// A window group that never binds: absent on both scopes, so `check` is
/// never called for it regardless of `span_ms`/`name`, which is why both are
/// left at the script's own no-op values (`0`, `""`).
fn push_absent_window(invocation: &mut redis::ScriptInvocation<'_>) {
    push_window(invocation, 0, "", false, None, false, None);
}

/// The reply tag (`"REFUSED"`/`"NONE"`), read the same way
/// `fair_use/scripts.rs`'s own `tag_of`/`str_at` do, duplicated here because
/// those are `pub(crate)` and this test invokes the script directly rather
/// than through [`roundhouse_store_redis::RedisFairUseLedger`].
fn reply_tag(reply: &[redis::Value]) -> Option<&str> {
    match reply.first()? {
        redis::Value::BulkString(bytes) => std::str::from_utf8(bytes).ok(),
        redis::Value::SimpleString(text) => Some(text.as_str()),
        _ => None,
    }
}

/// M13 thermo-nuclear review finding F6, closed: a window group past the ones
/// `FairUseWindow::ALL` names is read rather than silently skipped.
///
/// The finding was `for w = 1, 3` in `WOULD_EXCEED` against a Rust side that
/// derived its argument array from the enum: widening the enum would have
/// compiled, appended a fourth group, and left the new — typically widest —
/// window unsummed, answering `NONE` where the memory ledger refuses. The
/// script now takes its loop bound from the argument list it was handed.
///
/// This cannot be reached through [`WouldExceedArgs`], whose array is sized
/// from the enum — that is the *other* half of the fix, not a limitation of
/// the test — so it invokes the real script text through
/// [`fair_use_would_exceed_source`] with hand-built `ARGV`. The seam hands out
/// what ships: a copy of the Lua here would drift from `scripts.rs` and go on
/// passing while the real script regressed.
///
/// **The control matters as much as the claim.** The identical 30-day/1-token
/// window is proven to refuse from slot 1 first. The only thing that changes
/// between control and claim is *position* — slot 1 versus a fourth group
/// appended after three absent ones — which isolates the loop bound rather
/// than some mistake in how this test built its `ARGV`.
///
/// **What M13.1 changed here is what the fourth window has to be handed.**
/// The check now reads a running sum rather than scanning buckets, and
/// `record_draw` maintains one sum per window `FairUseWindow::ALL` names — so
/// a window the enum does not name has no sum until something writes one.
/// This test writes it, in the same shape and under the same field names the
/// script would have, which is exactly the state a fourth enum variant would
/// have put there by itself. The property under test is unchanged: whether
/// the group in the fourth slot is *read*.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_window_group_past_the_ones_the_enum_names_is_still_read() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");

    // now_ms far past the epoch so "10 days ago" does not clamp at zero, and
    // past `FairUseWindow::ALL`'s widest (7d) span so windows 1-3, even if
    // the loop somehow read the fourth group's data through them, could not
    // see this draw -- only a 30-day window can.
    let now_ms = 20 * 24 * 60 * 60_000u64;
    let at_ms = now_ms - 10 * 24 * 60 * 60_000;
    ledger.record_draw(&ada, at_ms, 50, 0.0).await.unwrap();

    let (project_prefix, member_prefix) = fair_use_scope_keys(&ada);
    // The fourth window's running sum, seeded exactly as `record_draw` seeds
    // the three the enum names: the draw above, covering the one bucket it
    // landed in. `bucket_index` is the ledger's own floor division, taken
    // through the same seam the storage assertions use rather than recomputed
    // here.
    let index: u64 = at_ms / BUCKET_MS;
    let _: () = redis::cmd("HSET")
        .arg(&project_prefix)
        .arg("s:30d_probe:t")
        .arg(50)
        .arg("s:30d_probe:u")
        .arg(0)
        .arg("s:30d_probe:from")
        .arg(index)
        .arg("s:30d_probe:to")
        .arg(index)
        .query_async(&mut raw)
        .await
        .unwrap();
    let thirty_days_ms = 30 * 24 * 60 * 60_000u64;
    let script = redis::Script::new(fair_use_would_exceed_source());

    let probe = |absent_before: usize| {
        let mut invocation = script.prepare_invoke();
        invocation
            .key(&project_prefix)
            .key(&member_prefix)
            .arg(now_ms)
            .arg(BUCKET_MS)
            .arg(FairUseScope::Project.wire_name())
            .arg(FairUseScope::Member.wire_name())
            .arg(FairUseQuantity::Tokens.wire_name())
            .arg(FairUseQuantity::Usd.wire_name())
            .arg(MAX_COUNT);
        for _ in 0..absent_before {
            push_absent_window(&mut invocation);
        }
        push_window(
            &mut invocation,
            thirty_days_ms,
            "30d_probe",
            true,
            Some(1),
            false,
            None,
        );
        for _ in 0..(FairUseWindow::ALL.len().saturating_sub(absent_before + 1)) {
            push_absent_window(&mut invocation);
        }
        invocation
    };

    // CONTROL: the 30-day/1-token window in the first slot, which every
    // plausible loop bound reads.
    let control_reply: Vec<redis::Value> = probe(0).invoke_async(&mut raw).await.unwrap();
    assert_eq!(
        reply_tag(&control_reply),
        Some("REFUSED"),
        "control: the 30-day/1-token window must refuse the 50-token draw \
         when placed in the first slot -- reply was {control_reply:?}"
    );

    // CLAIM: the same window appended *past* the groups `FairUseWindow::ALL`
    // names, exactly where a fourth window would land.
    let claim_reply: Vec<redis::Value> = probe(FairUseWindow::ALL.len())
        .invoke_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(
        reply_tag(&claim_reply),
        Some("REFUSED"),
        "a window group past the third must be summed like any other: the \
         script's loop bound is the argument list's length, so a window added \
         to FairUseWindow::ALL is checked rather than silently ignored -- \
         reply was {claim_reply:?}"
    );
}

/// **M13.1 review F6, closed: a draw whose bucket is below a window's
/// already-decayed `from` is not subtracted from that window's sum twice.**
///
/// `record_draw` used to lower `from` to a draw's bucket index whenever that
/// index was lower, with no decay on the write. If the window's decay had
/// already advanced `from` past that index once -- subtracting the bucket's
/// then-current value out of the running sum -- and the bucket's physical
/// fields were never pruned (only the widest window's decay deletes), an
/// out-of-order draw at that same low index dragged `from` back over ground
/// the sum no longer covered, and the next decay re-read the bucket's *total*
/// and subtracted all of it. The sum came out under-counted, in the
/// permissive direction, floored at zero so nothing complained.
///
/// The fix is the mark (R-F9): a draw older than a window's first bucket *at
/// the ledger's clock* is outside that window, so it neither joins the sum
/// nor moves `from` back -- which is exactly what the memory ledger does from
/// the other side, since its window is a range starting at that same first
/// bucket. The two are compared here as whole answers, and the stored sum is
/// read out of the raw hash as well, because an under-count of the running
/// sum is invisible in an admitted turn until the cap it clears is one it
/// should not have.
///
/// The fixture builds exactly the state that used to break: a first decay at
/// `first_decay_now` advances `from` past bucket 5 (subtracting its 10
/// tokens, leaving `from` at 6, the bucket itself unpruned since the
/// five-hour window is never the widest of the three `record_draw` tracks).
/// A second draw then lands back in bucket 5 -- below both that `from` and
/// the window's first bucket at the ledger's clock -- adding 90 more tokens
/// to the physical bucket. A second decay at `second_decay_now` then walks
/// `from` forward again over that bucket. Before the fix the sum was
/// credited with only the 90 and debited the whole 100, and the window came
/// out empty; now the 90 never joined the sum, `from` never went back, and
/// the walk finds the bucket already spent for. The memory ledger re-sums its
/// buckets from scratch and cannot make either mistake, so it is the control
/// the redis ledger is judged against.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn f6_a_draw_below_an_already_decayed_from_is_not_double_subtracted() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    // Five hours only: the narrower of the two windows `record_draw` always
    // maintains alongside the (never checked here) seven-day one, so its
    // decay in `would_exceed` never prunes. The cap sits strictly between the
    // correct final sum (60, bucket 40 alone) and the double-subtracted one
    // the finding described (60 - 100, floored at 0) -- so the two ledgers
    // agreeing on a refusal at the end is a direct sign the sum survived
    // rather than an incidental side effect of some other cap value.
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 55)]);
    let five_hours = FairUseWindow::FiveHours.span_ms();

    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        // Bucket 5: ten tokens, the value the first decay will subtract.
        ledger
            .record_draw(&ada, 5 * BUCKET_MS, 10, 0.0)
            .await
            .unwrap();
        // Bucket 40: sixty tokens, comfortably inside the window the whole
        // way through -- what should still be counted at the end.
        ledger
            .record_draw(&ada, 40 * BUCKET_MS, 60, 0.0)
            .await
            .unwrap();
    }
    let (project_key, _) = fair_use_scope_keys(&ada);

    // First decay: `now_ms` one bucket past bucket 5's departure from the
    // five-hour window, so `from` advances from 5 to 6 and the running sum
    // drops bucket 5's ten tokens -- 60 remain, all bucket 40's.
    let first_decay_now = five_hours + 5 * BUCKET_MS + BUCKET_MS;
    assert_eq!(
        redis
            .would_exceed(&ada, &terms, first_decay_now)
            .await
            .unwrap(),
        memory
            .would_exceed(&ada, &terms, first_decay_now)
            .await
            .unwrap(),
        "before the out-of-order draw the two ledgers must still agree"
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::FiveHours).await,
        Some((60, 0, 6, 40)),
        "the first decay subtracted exactly bucket 5's ten tokens and moved \
         `from` past it, leaving only bucket 40's sixty"
    );
    assert!(
        bucket_exists(&mut raw, &project_key, 5 * BUCKET_MS).await,
        "bucket 5's fields are not pruned: only the widest (seven-day) \
         window's decay deletes, and it was never asked anything here"
    );

    // The out-of-order draw: ninety more tokens land back in bucket 5, below
    // the `from` the first decay just advanced past it.
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger
            .record_draw(&ada, 5 * BUCKET_MS, 90, 0.0)
            .await
            .unwrap();
    }

    // Second decay: `first` must land strictly between bucket 5 (so the
    // subtract branch runs, not the read-free `to < first` reset -- `to` is
    // still 40, comfortably ahead) and bucket 40 (so bucket 40 is untouched).
    // `from` advances from 6 to 7 over an empty bucket, and bucket 5 -- which
    // now physically holds 10 + 90 tokens -- is below it and stays there.
    // This is the step that used to re-read that bucket and subtract all 100
    // of it out of a sum that had only ever been credited with 90.
    let second_decay_now = five_hours + 7 * BUCKET_MS;
    let memory_hit = memory
        .would_exceed(&ada, &terms, second_decay_now)
        .await
        .unwrap();
    let redis_hit = redis
        .would_exceed(&ada, &terms, second_decay_now)
        .await
        .unwrap();
    assert_eq!(
        memory_hit, redis_hit,
        "F6: a draw below an already-decayed `from` must not make the next \
         decay subtract bucket 5's now-100-token total out of a sum that was \
         never credited with it -- the two ledgers must agree at \
         {second_decay_now}, memory={memory_hit:?} redis={redis_hit:?}"
    );
    assert!(
        memory_hit.is_some(),
        "the true sum is bucket 40's 60 tokens against a cap of 55, so the \
         memory ledger -- which re-sums its buckets and cannot make this \
         mistake -- must refuse"
    );
    assert_eq!(
        window_sum(&mut raw, &project_key, FairUseWindow::FiveHours).await,
        Some((60, 0, 7, 40)),
        "F6: the five-hour window's running sum after the second decay must \
         still be bucket 40's sixty tokens -- 60 - 100 floored at zero would \
         show the double subtraction directly, and read back as 0 rather \
         than 60"
    );
}

/// **M13.1 review F8, closed: a draw stamped in a bucket newer than the
/// check's own clock is reached by the retry walk.**
///
/// The walk ran from the sum's oldest surviving bucket up to `now_index` --
/// the check's own clock, floored to a bucket -- dropping each in turn until
/// the remainder cleared every cap. A draw recorded at a timestamp a few
/// milliseconds past the checking node's clock (ordinary skew, straddling a
/// bucket boundary) still landed in the running sum, but the walk never
/// reached it: when that bucket was the only thing in the sum the loop's
/// range was empty and the answer fell through to `retry = now_ms`, an
/// immediate-retry 429 indistinguishable on the wire from "this window can
/// never have room". The memory ledger's walk is open-ended, reaches the
/// bucket, and returns its real departure time.
///
/// Under the mark (R-F9) the check is evaluated at the newest time the scope
/// has seen -- which the draw itself advanced -- so the walk's bound is past
/// the draw's bucket by construction rather than by a special case, and the
/// two ledgers name the same instant. The shared contract asserts the retry
/// time against each ledger on its own
/// (`a_draw_ahead_of_the_check_clock_is_reached_by_the_retry_walk`); this is
/// the differential.
///
/// A single draw five milliseconds into bucket 100, checked one millisecond
/// before bucket 100 begins: the draw is the only thing in the sum and is
/// unambiguously "the future" relative to the check's clock, so the retry
/// this produces has nothing else it could be answering.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn f8_a_draw_a_bucket_ahead_of_the_check_clock_is_reached_by_the_retry_walk() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);

    // Bucket 100, five milliseconds in -- the only draw in the sum.
    let draw_at = 100 * BUCKET_MS + 5;
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, draw_at, 100, 0.0).await.unwrap();
    }

    // One millisecond before bucket 100 begins: the draw above is stamped
    // inside bucket 100, which is therefore ahead of this check's own clock.
    let now_ms = 100 * BUCKET_MS - 1;
    let memory_hit = memory.would_exceed(&ada, &terms, now_ms).await.unwrap();
    let redis_hit = redis.would_exceed(&ada, &terms, now_ms).await.unwrap();

    assert!(
        memory_hit.is_some() && redis_hit.is_some(),
        "the single 100-token draw meets the 100-token cap on both ledgers, \
         so both must refuse -- memory={memory_hit:?} redis={redis_hit:?}"
    );
    assert_eq!(
        memory_hit, redis_hit,
        "F8: the retry walk must reach a bucket stamped newer than the \
         check's own clock rather than falling through to `retry = now_ms`, \
         which is the answer reserved for a window that can never have room; \
         memory={memory_hit:?} redis={redis_hit:?}"
    );
    assert_eq!(
        memory_hit.map(|hit| hit.retry_at_ms),
        Some(101 * BUCKET_MS + FairUseWindow::FiveHours.span_ms()),
        "and the instant they agree on is when the draw's own bucket leaves \
         the window -- not `now_ms`, which is what a walk bounded by the \
         checking clock returns while still looking like a refusal"
    );
}

/// **M13.1 review F9, closed: a check clock one millisecond behind an earlier
/// check agrees with the memory ledger, because both are evaluated at the
/// mark.**
///
/// `would_exceed` persists the decay it performs -- a window every bucket of
/// which has aged out has its sum `HDEL`ed -- so the Redis ledger cannot
/// answer a *later* call from an *earlier* clock by recomputing: the state
/// that answer would need is gone. The finding was that the memory ledger
/// (which re-sums what it holds) therefore refused where Redis admitted, in
/// the permissive direction, with a one-millisecond backwards step across a
/// boundary enough to reach it.
///
/// The ruling (R-F9) put the clock in the specification rather than making
/// the decay reversible: each scope's clock is the high-water mark of every
/// time handed to it, in *both* ledgers, so the second check below is
/// evaluated at the first check's `now_ms` and the two agree on `None`. The
/// shared contract asserts the rule against each ledger on its own
/// (`a_check_behind_an_earlier_one_is_evaluated_at_the_mark`); this is the
/// differential the contract cannot be, the same draws through both.
///
/// **Two controls, and neither is decoration.** The forward check at the
/// boundary is `scripts.rs`'s own worked example, and without it a ledger
/// that answered `None` to everything would pass. The second principal --
/// identical draw, identical instant, no clock history in front of it -- is
/// what proves the agreed `None` is the mark rather than a window that had
/// genuinely cleared for everybody.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn f9_a_check_clock_one_ms_behind_an_earlier_check_agrees_with_memory() {
    let memory = MemoryFairUseLedger::new();
    let redis = connect_fair_use_from_env().await;
    let ada = fresh_principal("ada");
    let terms = project_only(vec![tokens_cap(FairUseWindow::FiveHours, 100)]);

    // A single 100-token draw at the epoch -- meets the 100-token cap on the
    // five-hour window on both ledgers.
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&ada, 0, 100, 0.0).await.unwrap();
    }

    let span_ms = FairUseWindow::FiveHours.span_ms();
    let boundary = span_ms + BUCKET_MS;

    // CONTROL: a forward check landing on the boundary where the draw's
    // bucket has just aged out of the window. Both ledgers agree, and both
    // say the window is clear -- this is `scripts.rs`'s own worked example
    // and passes today. Without this control, a decay that always answered
    // `None` (rather than one that specifically forgets on a later, earlier
    // `now_ms`) would also pass the divergence assertion below for the wrong
    // reason.
    let memory_at_boundary = memory.would_exceed(&ada, &terms, boundary).await.unwrap();
    let redis_at_boundary = redis.would_exceed(&ada, &terms, boundary).await.unwrap();
    assert_eq!(
        redis_at_boundary, memory_at_boundary,
        "both ledgers must agree at the boundary itself"
    );
    assert_eq!(
        redis_at_boundary, None,
        "the draw has aged out of the five-hour window exactly at the \
         boundary, on both ledgers"
    );

    // The check clock now steps one millisecond BACKWARD across that same
    // boundary. Both ledgers answer at the mark the boundary check set, so
    // both answer exactly what it answered.
    let one_ms_earlier = boundary - 1;
    let memory_hit = memory
        .would_exceed(&ada, &terms, one_ms_earlier)
        .await
        .unwrap();
    let redis_hit = redis
        .would_exceed(&ada, &terms, one_ms_earlier)
        .await
        .unwrap();
    assert_eq!(
        redis_hit, memory_hit,
        "F9: a check clock that steps back across a boundary must not part \
         the two ledgers -- the Redis one persisted the boundary check's \
         decay and cannot recompute what it deleted, so both are defined to \
         answer at the mark; memory={memory_hit:?} redis={redis_hit:?}"
    );
    assert_eq!(
        redis_hit, None,
        "and the answer is the one the mark's instant gives, not the one \
         this call's own now_ms would have"
    );

    // CONTROL: the identical draw and the identical instant, on a principal
    // no check has ever taken past the boundary. Without it, a ledger that
    // had simply forgotten the draw would pass every assertion above.
    let bob = fresh_principal("bob");
    for ledger in [&memory as &dyn FairUseLedger, &redis] {
        ledger.record_draw(&bob, 0, 100, 0.0).await.unwrap();
    }
    let memory_bob = memory
        .would_exceed(&bob, &terms, one_ms_earlier)
        .await
        .unwrap();
    let redis_bob = redis
        .would_exceed(&bob, &terms, one_ms_earlier)
        .await
        .unwrap();
    assert_eq!(memory_bob, redis_bob, "the control must agree too");
    assert!(
        memory_bob.is_some(),
        "one millisecond before the boundary the draw is still inside the \
         five-hour window: the agreed None above is the mark at work"
    );
}
