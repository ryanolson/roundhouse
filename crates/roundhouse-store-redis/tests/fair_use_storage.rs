// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M13.1: the fair-use ledger's raw storage and round-trip cost, split out of
//! `fair_use_contract.rs` as this crate's own sibling-file convention asks
//! (see `recovery.rs`'s doc comment) once that file grew past the split's
//! themes (M13.1 review F1).
//!
//! **The two-scope update as storage**, read out of the raw hashes rather
//! than inferred from a refusal — and, since the M13 review, the two ways
//! that update can end: refused with neither scope moved, or saturated with
//! both. **One round trip per operation** is the other half: the claim the
//! whole key layout was chosen for, and the reason M13.1 exists at all — an
//! admitted turn used to scan every bucket in the widest configured window,
//! and now reads one running sum per (scope, window).
//!
//! Shared fixtures — the connection and the term builders both the decay and
//! storage tests need — live in `tests/common/fair_use.rs` rather than being
//! copied here a second time. The commandstats-counting helpers below stay
//! local: only the one round-trip test in this file uses them, and the
//! comment on that test explains why splitting its four measurements into
//! separate tests — which would need two of these loops running concurrently
//! against the same server-wide counter — is not an option (M13.1).
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.

mod common;

use roundhouse_core::control::fair_use::{BUCKET_MS, MAX_COUNT};
use roundhouse_core::control::spend::contract::fresh_principal;
use roundhouse_core::control::{FairUseLedger, FairUseTerms, FairUseWindow};
use roundhouse_store_redis::test_support::{
    fair_use_bucket_fields, fair_use_scope_keys, fair_use_window_sum_fields,
};

use common::fair_use::{connect_fair_use_from_env, tokens_cap};
use common::raw_from_env;

/// Script invocations the *server* has seen since the last reset.
///
/// The `redis` crate's `ConnectionManager` exposes no per-call counter, so the
/// count comes from `INFO commandstats` — which is server-wide, and therefore
/// only ever usable as a lower bound taken across attempts. See
/// [`every_operation_is_one_round_trip_and_an_unconfigured_one_is_none`] for
/// why that is still the right answer.
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
/// and `a_draw_past_the_widest_window_prunes_the_bucket_fields_it_ages_out` in
/// `fair_use_decay.rs`, which assert on the sum and bucket fields a disabled
/// reset would leave behind rather than on a read count.
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
