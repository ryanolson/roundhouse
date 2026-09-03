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
//! Below the suite are the claims a shared contract cannot make, because each
//! is about *sharing*, about *Redis*, or about the two backends *side by
//! side*:
//!
//! - **the unlock condition itself** — two handles over one Redis, a draw
//!   through one refused by the other, which is the whole reason this ledger
//!   exists;
//! - **the two-scope update as storage**, read out of the raw hashes rather
//!   than inferred from a refusal — and, since the M13 review, the two ways
//!   that update can end: refused with neither scope moved, or saturated with
//!   both;
//! - **the expiry**, in two halves: the TTL a production draw actually arms,
//!   and — through a `test-support` seam that shortens only the wait, never the
//!   policy — a stale bucket really disappearing and dropping out of the sum;
//! - **one round trip per operation**, the claim the whole key layout was
//!   chosen for;
//! - **the differentials**: the same draws through both ledgers at the
//!   boundaries where two arithmetics used to part company — a dollar cap met
//!   exactly, and the ceiling of the shared integer domain. The contract
//!   asserts each backend against the specification; these assert the two
//!   against *each other*, which is what a list of expected values cannot do
//!   once both lists can be edited;
//! - **a window group past the ones `FairUseWindow::ALL` names**, invoked
//!   against the real script text through a `test-support` seam, because
//!   `WouldExceedArgs` deliberately cannot express one.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

use roundhouse_core::control::fair_use::{BUCKET_MS, MAX_COUNT};
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{
    FairUseLedger, FairUseLimit, FairUseQuantity, FairUseScope, FairUseTerms, FairUseWindow,
    MemoryFairUseLedger,
};
use roundhouse_store_redis::RedisFairUseLedger;
use roundhouse_store_redis::test_support::{
    fair_use_bucket_fields, fair_use_scope_keys, fair_use_window_sum_fields,
    fair_use_would_exceed_source, url_from_env,
};

roundhouse_core::fair_use_ledger_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_fair_use_from_env().await
);

async fn connect_fair_use_from_env() -> RedisFairUseLedger {
    RedisFairUseLedger::connect(url_from_env())
        .await
        .expect("Redis named by the env var must be reachable")
}

async fn raw_from_env() -> redis::aio::MultiplexedConnection {
    redis::Client::open(url_from_env().as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
}

fn tokens_cap(window: FairUseWindow, max: u64) -> FairUseLimit {
    FairUseLimit {
        window,
        max_tokens: Some(max),
        max_usd: None,
    }
}

fn usd_cap(window: FairUseWindow, max: f64) -> FairUseLimit {
    FairUseLimit {
        window,
        max_tokens: None,
        max_usd: Some(max),
    }
}

fn project_only(limits: Vec<FairUseLimit>) -> FairUseTerms {
    FairUseTerms {
        project: limits,
        member: Vec::new(),
    }
}

/// Script invocations the *server* has seen since the last reset.
///
/// The `redis` crate's `ConnectionManager` exposes no per-call counter, so the
/// count comes from `INFO commandstats` — which is server-wide, and therefore
/// only ever usable as a lower bound taken across attempts. See
/// [`record_draw_and_would_exceed_are_single_round_trips`] for why that is
/// still the right answer.
async fn eval_calls_since_reset(raw: &mut redis::aio::MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(raw)
        .await
        .unwrap();
    info.lines()
        .filter_map(|line| {
            line.strip_prefix("cmdstat_eval:calls=")
                .or_else(|| line.strip_prefix("cmdstat_evalsha:calls="))
        })
        .filter_map(|rest| rest.split(',').next())
        .filter_map(|n| n.parse::<u64>().ok())
        .sum()
}

async fn reset_stats(raw: &mut redis::aio::MultiplexedConnection) {
    let _: () = redis::cmd("CONFIG")
        .arg("RESETSTAT")
        .query_async(raw)
        .await
        .unwrap();
}

/// `HMGET` calls the *server* has seen since the last reset -- the primitive
/// `would_exceed` issues once per (scope, window) it checks, plus once per
/// bucket range it has to walk. Same server-wide-counter caveat as
/// [`eval_calls_since_reset`]: a lower bound, read after a single attempt
/// with nothing else talking to this private Redis.
async fn hmget_calls_since_reset(raw: &mut redis::aio::MultiplexedConnection) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(raw)
        .await
        .unwrap();
    info.lines()
        .filter_map(|line| line.strip_prefix("cmdstat_hmget:calls="))
        .filter_map(|rest| rest.split(',').next())
        .filter_map(|n| n.parse::<u64>().ok())
        .sum()
}

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

/// One call moves both scopes' counters *in both shapes*, read out of the
/// storage itself.
///
/// The contract suite already asserts the *consequence* — a second member of
/// the project is refused by the project's window that the first filled. This
/// asserts the mechanism, because on this backend "both scopes" is two Redis
/// hashes written by one script, and a script that wrote one and not the other
/// would leave a member enforced against a project's counter. Reading the raw
/// fields is what tells those two apart.
///
/// **And it is the storage assertion M13.1 is actually about.** A draw writes
/// the bucket amount *and* every window's running sum, because the sum is what
/// a later admission compares against a cap without reading a bucket at all.
/// A ledger that wrote only the bucket fields would pass every behavioural
/// test in the contract — by re-scanning buckets on the read, which is the
/// path this rung replaced — and fail here.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn one_call_writes_both_scopes_buckets_and_every_windows_running_sum() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");

    // A draw landing squarely inside bucket 3, at a millisecond that is not
    // the bucket's own boundary: the index has to come from the floor
    // division rather than from the timestamp.
    let at_ms = 3 * BUCKET_MS + 17;
    ledger.record_draw(&ada, at_ms, 250, 1.5).await.unwrap();

    let (project_key, member_key) = fair_use_scope_keys(&ada);
    let (bucket_t, bucket_u) = fair_use_bucket_fields(at_ms);
    for (key, whose) in [(&project_key, "the project's"), (&member_key, "ada's own")] {
        let fields: Vec<Option<String>> = redis::cmd("HMGET")
            .arg(key)
            .arg(&bucket_t)
            .arg(&bucket_u)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(
            fields,
            vec![Some("250".to_string()), Some("1500000".to_string())],
            "{whose} bucket holds the tokens and the dollars as micro-dollars"
        );

        // Every window's running sum, including the two nobody has configured
        // — a draw has no terms, and an admin PATCH can start enforcing any of
        // them a minute later.
        for window in FairUseWindow::ALL {
            let (sum_t, sum_u, from, to) = fair_use_window_sum_fields(window);
            let fields: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(key)
                .arg(&sum_t)
                .arg(&sum_u)
                .arg(&from)
                .arg(&to)
                .query_async(&mut raw)
                .await
                .unwrap();
            assert_eq!(
                fields,
                vec![
                    Some("250".to_string()),
                    Some("1500000".to_string()),
                    Some("3".to_string()),
                    Some("3".to_string()),
                ],
                "{whose} {} window carries the same draw as a running sum, \
                 covering exactly bucket 3",
                window.wire_name()
            );
        }
    }

    // CONTROL: the neighbouring bucket was not touched. Without it, a script
    // that wrote every bucket in sight would pass the assertions above.
    let (neighbour_t, _) = fair_use_bucket_fields(at_ms + BUCKET_MS);
    let exists: bool = redis::cmd("HEXISTS")
        .arg(&project_key)
        .arg(&neighbour_t)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(!exists, "only the bucket the draw landed in exists");
}

/// One window's persisted running sum, as `(tokens, micros, from, to)`.
///
/// `None` where the window carries no sum at all, which is a state the decay
/// really does write: a window every draw it covered has aged out of is
/// *deleted* rather than zeroed, so an untouched window and a fully-decayed
/// one are one state rather than two spellings a later read has to tell
/// apart.
async fn window_sum(
    raw: &mut redis::aio::MultiplexedConnection,
    key: &str,
    window: FairUseWindow,
) -> Option<(u64, u64, u64, u64)> {
    let (t, u, from, to) = fair_use_window_sum_fields(window);
    let fields: Vec<Option<String>> = redis::cmd("HMGET")
        .arg(key)
        .arg(&t)
        .arg(&u)
        .arg(&from)
        .arg(&to)
        .query_async(raw)
        .await
        .unwrap();
    let read = |at: usize| fields[at].as_ref().map(|text| text.parse::<u64>().unwrap());
    Some((read(0)?, read(1)?, read(2)?, read(3)?))
}

async fn bucket_exists(raw: &mut redis::aio::MultiplexedConnection, key: &str, at_ms: u64) -> bool {
    let (t, _) = fair_use_bucket_fields(at_ms);
    redis::cmd("HEXISTS")
        .arg(key)
        .arg(&t)
        .query_async(raw)
        .await
        .unwrap()
}

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

/// Each trait method is one script and therefore one round trip — and a
/// membership with no windows is no round trip at all.
///
/// Counts `EVAL`/`EVALSHA` calls between `CONFIG RESETSTAT`-bounded points on
/// the *server*, taking the minimum over many attempts: the same technique,
/// and the same reasoning, as
/// `open_grant_and_settle_grant_are_single_round_trips` in `spend_contract.rs`.
/// The counter is server-wide, so a test running concurrently can only *add*
/// calls this one did not make, never hide one it did, and the minimum across
/// attempts is the attempt that raced nobody.
///
/// **All four measurements live in one test on purpose.** That argument holds
/// only while the competing traffic is sparse relative to the measurement
/// window; two such counting loops running concurrently are each other's dense
/// competitor, and a first draft that split this in two failed for exactly
/// that reason — reliably, not occasionally. One measuring loop per test
/// binary is the invariant, and this comment is where it is written down.
///
/// **The fourth measurement is what M13.1 is for.** M13's review (F4) pinned
/// what the bucket-per-key scan cost an *admitted* turn — the common case,
/// and the one where no window binds, so the scan widened through every
/// configured window to the widest: 2017 `HMGET`s per capped scope, 4034 for
/// a membership capped on both. The running sums replace that scan, and this
/// assertion is where the drop is proved rather than claimed: an admitted
/// turn now costs **one `HMGET` per (scope, window) checked** and nothing per
/// bucket, because the sum it compares against the cap was maintained on
/// write. Derived from `FairUseWindow::ALL`, not pasted, so a fourth window
/// moves it by construction.
///
/// **This measurement is the decay-free steady state on purpose, and does not
/// itself guard `decay`'s zero-read reset branch** (M13.1 refute F1): the
/// `steady` fixture below lands its draw in the bucket right before the
/// check so nothing has aged out, because that no-decay case is what an
/// admitted turn actually costs in production. A `to < first` reset costs the
/// same one `HMGET` this measurement already pins — the branch it disables
/// only shows up as *extra* reads on a scope with something to age out, which
/// this fixture deliberately has none of. That branch's removal is caught
/// instead by `an_idle_past_the_narrowest_window_drops_its_sum_and_keeps_the_widest`
/// and `a_draw_past_the_widest_window_prunes_the_bucket_fields_it_ages_out`,
/// which assert on the sum and bucket fields a disabled reset would leave
/// behind rather than on a read count.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn every_operation_is_one_round_trip_and_an_unconfigured_one_is_none() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    // All three windows capped on both scopes: the widest, most-scanning shape
    // this ledger can be asked for, and still one round trip.
    let terms = FairUseTerms {
        project: FairUseWindow::ALL
            .iter()
            .map(|window| tokens_cap(*window, 1_000_000))
            .collect(),
        member: FairUseWindow::ALL
            .iter()
            .map(|window| tokens_cap(*window, 1_000_000))
            .collect(),
    };
    // The admitted turn the read-count pin is about, given its own principal
    // and its own instant so the measurement is the steady state rather than
    // whatever the loop above happened to leave behind. `now_ms` is past the
    // widest window's span, so no window's scan clamps at the epoch and every
    // one of them is a full-width window; the draw lands in the bucket before
    // it, so nothing has aged out and the read is exactly the decay-free path
    // an admitted turn takes.
    let steady = fresh_principal("steady");
    let steady_now_ms = FairUseWindow::SevenDays.span_ms() + BUCKET_MS;

    const ATTEMPTS: u64 = 25;
    let (mut draw, mut check, mut unconfigured, mut admitted_hmgets) =
        (u64::MAX, u64::MAX, u64::MAX, u64::MAX);
    for attempt in 0..ATTEMPTS {
        reset_stats(&mut raw).await;
        ledger
            .record_draw(&ada, attempt * BUCKET_MS, 1, 0.001)
            .await
            .unwrap();
        draw = draw.min(eval_calls_since_reset(&mut raw).await);

        reset_stats(&mut raw).await;
        ledger
            .would_exceed(&ada, &terms, ATTEMPTS * BUCKET_MS)
            .await
            .unwrap();
        check = check.min(eval_calls_since_reset(&mut raw).await);

        reset_stats(&mut raw).await;
        assert_eq!(
            ledger
                .would_exceed(&ada, &FairUseTerms::default(), ATTEMPTS * BUCKET_MS)
                .await
                .unwrap(),
            None
        );
        unconfigured = unconfigured.min(eval_calls_since_reset(&mut raw).await);

        ledger
            .record_draw(&steady, steady_now_ms - BUCKET_MS, 1, 0.001)
            .await
            .unwrap();
        reset_stats(&mut raw).await;
        assert_eq!(
            ledger
                .would_exceed(&steady, &terms, steady_now_ms)
                .await
                .unwrap(),
            None,
            "the handful of tokens drawn above are nowhere near the 1,000,000 \
             cap, so every window has room -- this is the admitted turn whose \
             read cost the whole rung is about"
        );
        admitted_hmgets = admitted_hmgets.min(hmget_calls_since_reset(&mut raw).await);
    }

    assert_eq!(
        draw, 1,
        "record_draw updates both scopes and arms both expiries in one script"
    );
    assert_eq!(
        check, 1,
        "would_exceed sums every window over both scopes in one script, however \
         many buckets that is"
    );
    // The shipped posture: every project has no fair-use block until an
    // operator writes one down, so the admission path of every turn in such a
    // deployment must cost nothing at all rather than a round trip that comes
    // back saying "nothing configured". The `check` measurement above is this
    // one's control — the identical call under configured terms does reach the
    // script, so a ledger that had simply stopped talking to Redis would fail
    // there rather than pass here.
    assert_eq!(
        unconfigured, 0,
        "a membership with no windows is answered without asking Redis anything"
    );
    // M13.1's pin, derived rather than pasted: one read of one window's
    // running sum per capped scope, and no read per bucket at all. Under the
    // layout this rung replaced the same call cost `2 * 2017` — the widest
    // window's own width, twice — which is what made this assertion the red
    // test the redesign had to turn green.
    let expected_hmgets = 2 * FairUseWindow::ALL.len() as u64; // project scope + member scope
    assert_eq!(
        admitted_hmgets,
        expected_hmgets,
        "an admitted turn reads one running sum per (scope, window) and walks \
         no buckets: expected {expected_hmgets} HMGETs (2 scopes x \
         {} windows), not the 2017-per-scope bucket scan the bucket-per-key \
         layout paid on every admission (M13 review, F4)",
        FairUseWindow::ALL.len()
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

/// M13 thermo-nuclear review finding F5, closed: a draw either moves both
/// scopes or moves neither, with no third outcome left.
///
/// The finding was that `RECORD_DRAW` claimed its writes were "one indivisible
/// step" while Redis Lua does not roll back writes made before a command
/// error: an `HINCRBY` overflowing on the *member*'s bucket aborted the script
/// with the *project*'s bucket already incremented and its TTL re-armed. What
/// closed it is that there is no failing command any more — the script reads,
/// adds with a clamp at `MAX_COUNT` and writes back, and the only draw that
/// can be refused is refused in Rust before the script runs at all.
///
/// Both halves are asserted through the raw hashes rather than through a
/// refusal, because storage is what the finding was about: a refused draw
/// leaves both buckets unset, and a draw that carries a counter past the
/// domain leaves *both* scopes at the ceiling rather than one moved and one
/// not.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_refused_draw_moves_neither_scope_and_a_saturating_one_moves_both() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");

    // Bucket 0: at_ms = 0 lands squarely in it, no boundary ambiguity.
    let at_ms = 0;
    let (project_key, member_key) = fair_use_scope_keys(&ada);
    let (bucket_t, bucket_u) = fair_use_bucket_fields(at_ms);

    // A draw one token outside the domain: refused at the edge, in Rust.
    assert!(
        ledger
            .record_draw(&ada, at_ms, MAX_COUNT + 1, 0.0)
            .await
            .is_err(),
        "a count no ledger can hold exactly is refused rather than recorded"
    );
    for key in [&project_key, &member_key] {
        let exists: bool = redis::cmd("EXISTS")
            .arg(key)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert!(
            !exists,
            "a refused draw leaves {key} exactly as it was -- absent -- and \
             in particular does not leave the project's hash carrying a \
             draw the member's never got"
        );
    }

    // Two draws that together leave the domain. Neither is refused; both
    // scopes saturate, together.
    ledger
        .record_draw(&ada, at_ms, MAX_COUNT, 0.0)
        .await
        .unwrap();
    ledger
        .record_draw(&ada, at_ms, MAX_COUNT, 0.0)
        .await
        .unwrap();
    for key in [&project_key, &member_key] {
        let fields: Vec<Option<String>> = redis::cmd("HMGET")
            .arg(key)
            .arg(&bucket_t)
            .arg(&bucket_u)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(
            fields,
            vec![Some(MAX_COUNT.to_string()), Some("0".to_string())],
            "{key} saturates at the domain ceiling rather than wrapping, \
             erroring, or being written in scientific notation by Lua's own \
             number-to-string conversion"
        );
    }
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
/// than through [`RedisFairUseLedger`].
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
