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
//! Below the suite are the four claims a shared contract cannot make, because
//! each is about *sharing* or about *Redis*:
//!
//! - **the unlock condition itself** — two handles over one Redis, a draw
//!   through one refused by the other, which is the whole reason this ledger
//!   exists;
//! - **the two-scope update as storage**, read out of the raw hashes rather
//!   than inferred from a refusal;
//! - **the expiry**, in two halves: the TTL a production draw actually arms,
//!   and — through a `test-support` seam that shortens only the wait, never the
//!   policy — a stale bucket really disappearing and dropping out of the sum;
//! - **one round trip per operation**, the claim the whole key layout was
//!   chosen for.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

use roundhouse_core::control::fair_use::BUCKET_MS;
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{
    FairUseLedger, FairUseLimit, FairUseScope, FairUseTerms, FairUseWindow,
};
use roundhouse_store_redis::RedisFairUseLedger;
use roundhouse_store_redis::test_support::{fair_use_bucket_keys, url_from_env};

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

/// One call moves both scopes' counters, read out of the storage itself.
///
/// The contract suite already asserts the *consequence* — a second member of
/// the project is refused by the project's window that the first filled. This
/// asserts the mechanism, because on this backend "both scopes" is two Redis
/// keys written by one script, and a script that wrote one and not the other
/// would leave a member enforced against a project's counter. Reading the raw
/// hashes is what tells those two apart.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn one_call_writes_both_scopes_buckets() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");

    // A draw landing squarely inside bucket 3, at a millisecond that is not
    // the bucket's own boundary: the index has to come from the floor
    // division rather than from the timestamp.
    let at_ms = 3 * BUCKET_MS + 17;
    ledger.record_draw(&ada, at_ms, 250, 1.5).await.unwrap();

    let (project_key, member_key) = fair_use_bucket_keys(&ada, at_ms);
    for (key, whose) in [(&project_key, "the project's"), (&member_key, "ada's own")] {
        let fields: Vec<Option<String>> = redis::cmd("HMGET")
            .arg(key)
            .arg("t")
            .arg("u")
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(
            fields,
            vec![Some("250".to_string()), Some("1500000".to_string())],
            "{whose} bucket holds the tokens and the dollars as micro-dollars"
        );
    }

    // CONTROL: the neighbouring bucket was not touched. Without it, a script
    // that wrote every bucket in sight would pass the assertions above.
    let (neighbour, _) = fair_use_bucket_keys(&ada, at_ms + BUCKET_MS);
    let exists: bool = redis::cmd("EXISTS")
        .arg(&neighbour)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(!exists, "only the bucket the draw landed in exists");
}

/// The expiry a real draw arms is the derived one: the widest window plus one
/// bucket.
///
/// No sleeping and no seam — `PTTL` answers directly. This is the half of the
/// expiry story that is about *policy*, and it is the half a shortened TTL
/// could not check.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_bucket_is_armed_to_expire_one_bucket_past_the_widest_window() {
    let ledger = connect_fair_use_from_env().await;
    let mut raw = raw_from_env().await;
    let ada = fresh_principal("ada");
    ledger.record_draw(&ada, 0, 1, 0.0).await.unwrap();

    let expected = FairUseWindow::SevenDays.span_ms() + BUCKET_MS;
    let (project_key, member_key) = fair_use_bucket_keys(&ada, 0);
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
            "a bucket must outlive the widest window by one bucket width; \
             PTTL was {pttl}, expected about {expected}"
        );
    }
}

/// A bucket that has expired stops counting, watched happening.
///
/// **Why this sleeps when nothing else in the suite does.** Every other
/// time-dependent assertion is reached by supplying a later `now_ms`, which is
/// exactly what the caller-supplied clock is for. Redis key expiry is the one
/// clock this ledger does *not* own: it is the server's, and it is what
/// replaces the pruning pass the rejected layout needed. The only way to watch
/// a key actually leave is to wait for it — so the `test-support` seam shortens
/// the wait to a fraction of a second while the production policy, asserted
/// directly above, stays derived from the window widths.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn an_expired_bucket_leaves_the_sum_as_well_as_the_keyspace() {
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

    let (project_key, _) = fair_use_bucket_keys(&ada, 0);
    let exists: bool = redis::cmd("EXISTS")
        .arg(&project_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert!(
        !exists,
        "Redis is the pruning pass; nothing else deletes a bucket"
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
/// **All three measurements live in one test on purpose.** That argument holds
/// only while the competing traffic is sparse relative to the measurement
/// window; two such counting loops running concurrently are each other's dense
/// competitor, and a first draft that split this in two failed for exactly
/// that reason — reliably, not occasionally. One measuring loop per test
/// binary is the invariant, and this comment is where it is written down.
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

    const ATTEMPTS: u64 = 25;
    let (mut draw, mut check, mut unconfigured) = (u64::MAX, u64::MAX, u64::MAX);
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
}
