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
    MemoryFairUseLedger, Principal,
};
use roundhouse_store_redis::RedisFairUseLedger;
use roundhouse_store_redis::test_support::{
    fair_use_bucket_keys, fair_use_would_exceed_source, url_from_env,
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
/// `widen` (`fair_use/scripts.rs:116-132`) issues once per bucket index it
/// walks, present or empty. Same server-wide-counter caveat as
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
///
/// **A fourth measurement lives here too, for the same reason (M13 review,
/// F4).** The module doc and `WOULD_EXCEED`'s own doc comment used to call 61
/// reads "the common case," reasoning that the narrowest (5-hour) window binds
/// first. But `check` only stops early by *finding* a refusal, and an admitted
/// turn — the case a fleet serves most of the time — is exactly the one where
/// no window ever binds, so the scan in `widen` runs to the end of every
/// present window rather than stopping at the first. This test's `terms` is
/// already the widest-scanning shape the ledger takes (all three windows,
/// both scopes); reusing it at a `now_ms` past the widest window's span, with
/// nothing drawn large enough to bind any cap, is an admitted turn under that
/// shape, and `hmget_calls_since_reset` pins what it actually costs. It could
/// not live in its own `#[tokio::test]` without becoming the second
/// `commandstats`-measuring loop the paragraph above rules out.
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
    // Past the widest window, so none of the three scans clamp at the epoch
    // and every configured window runs its full width -- the shape an
    // admitted turn's scan actually takes (F4).
    let widen_now_ms = FairUseWindow::SevenDays.span_ms() + BUCKET_MS;

    const ATTEMPTS: u64 = 25;
    let (mut draw, mut check, mut unconfigured, mut widen_hmgets) =
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

        reset_stats(&mut raw).await;
        assert_eq!(
            ledger
                .would_exceed(&ada, &terms, widen_now_ms)
                .await
                .unwrap(),
            None,
            "the handful of tokens drawn above are nowhere near the 1,000,000 \
             cap, so every window has room -- this is the admission the doc \
             used to call the common case's 61-read scan"
        );
        widen_hmgets = widen_hmgets.min(hmget_calls_since_reset(&mut raw).await);
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
    // F4's pin, derived rather than pasted: the 7-day window's own width is
    // the number of buckets *any* window's scan reaches once it is the widest
    // one still unbound, and an admitted turn under an all-three-windows
    // membership reaches exactly it, on both scopes. When M13.1 replaces this
    // scan with running sums maintained on write, this assertion is the red
    // test that proves the read count actually dropped.
    let widest_window_buckets = FairUseWindow::SevenDays.span_ms() / BUCKET_MS + 1;
    let expected_widen_hmgets = 2 * widest_window_buckets; // project scope + member scope
    assert_eq!(
        widen_hmgets, expected_widen_hmgets,
        "an admitted turn's scan widens through every configured window to \
         the widest one instead of stopping at the narrowest (M13 review, \
         F4): expected {expected_widen_hmgets} HMGETs (2 scopes x the 7-day \
         window's {widest_window_buckets} buckets), not the 61-per-scope a \
         *refusal* at the narrowest window would cost"
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
    let (project_key, member_key) = fair_use_bucket_keys(&ada, at_ms);

    // A draw one token outside the domain: refused at the edge, in Rust.
    assert!(
        ledger
            .record_draw(&ada, at_ms, MAX_COUNT + 1, 0.0)
            .await
            .is_err(),
        "a count no ledger can hold exactly is refused rather than recorded"
    );
    for key in [&project_key, &member_key] {
        let fields: Vec<Option<String>> = redis::cmd("HMGET")
            .arg(key)
            .arg("t")
            .arg("u")
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(
            fields,
            vec![None, None],
            "a refused draw leaves {key} exactly as it was -- unset -- and \
             in particular does not leave the project's bucket carrying a \
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
            .arg("t")
            .arg("u")
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

/// The prefix each of a principal's two bucket-key streams shares -- what
/// `KEYS[1]`/`KEYS[2]` are in `WOULD_EXCEED`, before the script appends
/// `:<index>` itself. Recovered from [`fair_use_bucket_keys`] at index 0
/// (`prefix:0`) rather than duplicating `bucket_key`'s format here, so this
/// stays pinned to the same construction the shared contract suite already
/// exercises.
fn bucket_prefixes(principal: &Principal) -> (String, String) {
    let (project_key, member_key) = fair_use_bucket_keys(principal, 0);
    let strip = |key: String| {
        key.strip_suffix(":0")
            .expect("fair_use_bucket_keys(_, 0) must end in the index it was given")
            .to_string()
    };
    (strip(project_key), strip(member_key))
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

    let (project_prefix, member_prefix) = bucket_prefixes(&ada);
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
