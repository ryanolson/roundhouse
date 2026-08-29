// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review finding F2 (M11.0 thermo-nuclear round): "`ProviderPricing::price()`
//! became non-additive under the measured cache-write split, but the metrics
//! rollup and admin reconciliation view price a summed `Usage` under an
//! explicit, now-false 'linear in tokens' assumption."
//!
//! `price()` (`routing::ledger`) takes the conservative all-uncached-at-write-
//! rate branch only when a call reports *no* measured cache write; a call that
//! reports one splits its uncached share between the write rate and the plain
//! input rate. That makes `price(a) + price(b) != price(a + b)` (`a + b` via
//! `Usage::add`) whenever one call in a pot reports a write and another does
//! not — ordinary Anthropic traffic, since a prompt under the cacheable
//! minimum never sets `cache_creation_input_tokens` while a longer one does.
//!
//! Two call sites priced a *summed* `Usage` on exactly that assumption:
//! `metrics::fold::Counted` accumulated every call's `Usage` into one row via
//! `Usage::add` before `metrics::snapshot::MetricsSnapshot::build` priced the
//! row once, both saying "`price` is linear in tokens" where the sum happened.
//! Meanwhile `roundhouse-server`'s `engine::spend::settled_cost_usd` prices
//! each turn's `Usage` individually and `control::spend` sums the *dollars*
//! into `Account::committed_usd`. `admin_api::reconciliation` reads the first
//! path as `measured_usd` and the second as `committed_usd`, and publishes
//! their difference as `drift_usd` — documented as having exactly three causes,
//! none of which was a pricing-function artifact.
//!
//! This file stays inside `roundhouse-core` (`committed_usd`'s accrual lives in
//! `roundhouse-server`) and demonstrates the mechanism two ways: first as a
//! standalone property of `ProviderPricing::price` and `Usage::add`, then
//! end to end through the same `MetricsFold` / `MetricsSnapshot` pipeline
//! `/v1/metrics` and reconciliation's `measured_usd` actually run, comparing
//! its output against what turn-by-turn settlement (the arithmetic
//! `engine::spend::settled_cost_usd` + `Account::committed_usd` perform, traced
//! by hand since that accrual lives in a different crate) would have produced
//! for the identical two calls.
//!
//! **Ruled and fixed; kept as the regression.** The finding is valid, and the
//! remedy its wording implies is not the one taken: restoring
//! `price(a) + price(b) == price(a + b)` on a summed `Usage` is only reachable
//! by abandoning `price`'s conservative unmeasured branch, which errs downwards
//! on purpose. So `price` stays non-additive over a sum — the first test now
//! *pins* that, so the branch cannot be flattened in linearity's name — and no
//! rollup prices a sum any more. `routing::PooledUsage` takes each call's
//! cache-write split as the call is booked and accumulates only the result, so
//! `metrics::fold::Counted` pools rather than sums and
//! `MetricsSnapshot::build` prices through `ProviderPricing::price_pooled`.
//! "The rollup's dollars are the sum of the per-turn dollars" is now a property
//! of the accumulator rather than an assumption about the formula.
//!
//! Uses only the public API of `roundhouse-core`. The library gained
//! `PooledUsage` and `price_pooled` as the fix; nothing was widened merely to
//! make these tests possible.

use roundhouse_core::event::{Accounting, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::metrics::pricing::{ReferenceModel, ShadowPricing};
use roundhouse_core::metrics::{MetricsConfig, MetricsFold, MetricsSnapshot, Scope};
use roundhouse_core::routing::{DecisionRecord, PooledUsage, ProviderPricing, Target};

/// A Claude-shaped rate card, deliberately identical to `routing::ledger`'s own
/// `CLAUDE` test fixture so the dollar figures here can be checked against
/// that module's comments: the read is a tenth of input, the write is 1.25x
/// it, and all four figures differ so a term billed at the wrong rate cannot
/// cancel out.
const CLAUDE: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

fn frontier() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude".into(),
    }
}

/// One reported call: `uncached` total uncached input tokens (`cached_input_tokens`
/// stays 0, so `input_tokens == uncached`), of which `write` were measured as a
/// fresh cache write -- `write` is a *subset* of `uncached`, per `price()`'s own
/// clamp (`written = usage.cache_write_tokens.min(uncached)`), never additional
/// to it. Mirrors the shape Anthropic traffic actually takes: a prompt under the
/// cacheable minimum ignores the breakpoint and reports no
/// `cache_creation_input_tokens` at all (`write: 0`), while a longer turn reports
/// one for its whole uncached share (`write == uncached`).
fn call(uncached: u64, write: u64) -> Usage {
    assert!(write <= uncached, "write must be a subset of uncached");
    Usage {
        input_tokens: uncached,
        cached_input_tokens: 0,
        cache_write_tokens: write,
        output_tokens: 0,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

fn config() -> MetricsConfig {
    MetricsConfig::new(ShadowPricing::new(vec![ReferenceModel {
        provider: "anthropic".into(),
        model: "claude".into(),
        pricing: CLAUDE,
        quality_prior: 0.6,
    }]))
}

/// A log of hosted calls, each routed and completed, all to the same model.
/// Copied from `finding2_provenance_laundering.rs`'s own `log` helper:
/// constructing a `TurnStarted`/`Routed`/`ResponseCompleted` triple by hand is
/// the only way to drive `MetricsFold` through its public surface.
fn log(calls: &[Usage]) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let session = SessionId::new("s1");
    let mut at_ms = 1_000u64;
    let mut push = |events: &mut Vec<SessionEvent>, kind: SessionEventKind| {
        at_ms += 10;
        let seq = events.len() as u64 + 1;
        events.push(SessionEvent {
            seq,
            session_id: session.clone(),
            at_ms,
            kind,
        });
    };
    for (i, usage) in calls.iter().enumerate() {
        let response_id = ResponseId::new(format!("r{i}"));
        push(
            &mut events,
            SessionEventKind::TurnStarted {
                turn_id: TurnId::new(format!("t{i}")),
                response_id: response_id.clone(),
            },
        );
        push(
            &mut events,
            SessionEventKind::Routed {
                response_id: response_id.clone(),
                decision: DecisionRecord {
                    chosen: frontier(),
                    rationale: "test".into(),
                    policy: "test".into(),
                    isl_tokens: usage.input_tokens,
                    expected_prefill_tokens: 0.0,
                    expected_cost_usd: 0.0,
                    considered: vec![],
                    turn_policy_digest: String::new(),
                    budget_state: Default::default(),
                    rate_card: None,
                    payer: Default::default(),
                    billing: Default::default(),
                    budget_draw: None,
                    withheld_providers: Vec::new(),
                    declared_baseline: None,
                    attempts: Vec::new(),
                },
            },
        );
        push(
            &mut events,
            SessionEventKind::ResponseCompleted {
                response_id,
                usage: usage.clone(),
                provider_reported_cost_usd: None,
            },
        );
    }
    events
}

fn snapshot_of(calls: &[Usage]) -> MetricsSnapshot {
    let mut fold = MetricsFold::new();
    fold.extend(log(calls).iter());
    MetricsSnapshot::build(&fold, Scope::Deployment, &config(), 9_999)
}

/// **The root mechanism, and the remedy that answers it.** `price()` is
/// additive on a turn with no measured write and additive on a turn whose whole
/// uncached share was measured, but not additive across the two once combined by
/// `Usage::add` — the exact operation `metrics::fold::Counted::add` used to
/// perform on every call that landed in the same `(provider, model)` row.
///
/// Numbers match review finding F2's own arithmetic against the `CLAUDE`
/// fixture: turn A is a short prompt under Anthropic's cacheable minimum (no
/// write measured), turn B is a longer turn whose whole uncached share was
/// written.
///
/// **This test asserts the invariant, not the property F2's own wording implied.**
/// F2 read the defect as "`price` stopped being linear in tokens", and the
/// obvious repair — make `price(a) + price(b) == price(a + b)` hold again on a
/// summed `Usage` — is the one the M11.0 review ruling refused: the two calls
/// disagree about whether a write was measured, and the only way a *sum* can
/// answer for both is to abandon the conservative unmeasured branch, which is
/// the safe direction and load-bearing elsewhere. So the sum stays
/// non-additive, deliberately and permanently — pinned below so nobody
/// "restores linearity" by flattening that branch — and the rollup stops
/// pricing sums. `PooledUsage` takes each call's split on the way in and
/// accumulates only the result, which makes additivity a property of the
/// accumulator instead of a hope about the formula.
#[test]
fn pooling_per_call_prices_a_measured_and_an_unmeasured_turn_additively() {
    let a = call(1_000, 0);
    let b = call(2_000, 2_000);

    let price_a = CLAUDE.price(&a);
    let price_b = CLAUDE.price(&b);
    // 1,000 uncached tokens, nothing measured -> whole share at the write
    // rate: 1,000 * 3.75e-6.
    assert!((price_a - 0.00_375).abs() < 1e-9, "price_a = {price_a}");
    // 2,000 uncached tokens, all 2,000 measured as a write -> the same write
    // rate, nothing left over for the plain rate: 2,000 * 3.75e-6.
    assert!((price_b - 0.00_750).abs() < 1e-9, "price_b = {price_b}");
    let sum_of_prices = price_a + price_b;

    // **The pot the fold now keeps.** Each call's cache-write share is decided
    // under its own branch as it is booked, so the pot prices for exactly what
    // the two calls priced for.
    let mut pooled = PooledUsage::of(&a);
    pooled.add(&b);
    let price_of_pool = CLAUDE.price_pooled(&pooled);
    assert!(
        (sum_of_prices - price_of_pool).abs() < 1e-9,
        "price(a) + price(b) = ${sum_of_prices:.6} but the pot of the two prices \
         ${price_of_pool:.6} -- pooling must accumulate each call's own cache-write \
         decision, not re-decide it over the sum",
    );

    // **The defect, pinned as the reason the pot exists.** Turn A's 1,000
    // tokens are individually priced at the premium write rate (nothing
    // measured them), but summing the `Usage` first hands turn B's positive
    // write signal to the whole pot, and 1,000 of its 3,000 uncached tokens get
    // repriced at the cheaper plain rate they were never entitled to. This is
    // the arithmetic every summed-`Usage` call site in F2 leaned on -- "price
    // is linear in tokens", said by `metrics::fold` and `metrics::snapshot`
    // alike until this finding -- and it does not hold.
    let mut summed = a.clone();
    summed.add(&b);
    let price_of_sum = CLAUDE.price(&summed);
    assert!(
        (sum_of_prices - price_of_sum - 0.00_075).abs() < 1e-9,
        "summing the Usage before pricing must still understate the per-turn total by \
         exactly $0.000750 here: price(a) + price(b) = ${sum_of_prices:.6}, \
         price(a + b) = ${price_of_sum:.6}. If this now agrees, the conservative \
         unmeasured branch has been flattened and every unmeasured turn on record has \
         been quietly re-priced downwards -- which inflates the saving.",
    );
}

/// **The consequence, through the real pipeline.** The metrics rollup — the
/// exact machinery that publishes `Savings::frontier_spend_usd`, which
/// `admin_api::reconciliation` reads as `measured_usd` and subtracts from
/// `committed_usd` to report `drift_usd` — used to take the losing side of that
/// non-additivity, under-reporting these two calls by $0.000750.
///
/// `committed_usd` is accrued turn by turn: `engine::spend::settled_cost_usd`
/// (in `roundhouse-server`, not reachable from this crate) prices one
/// settlement's `Usage` at a time via `card.price(&settlement.usage)`, and
/// `control::spend::Account::commit` sums the resulting dollars. For these two
/// calls that is exactly `CLAUDE.price(&a) + CLAUDE.price(&b)` — reproduced
/// here by hand since the accrual itself lives in `roundhouse-server`.
/// `measured_usd` is `Savings::frontier_spend_usd`, and it used to be read off
/// a `Usage` that `MetricsFold` had summed via `Usage::add` *before*
/// `MetricsSnapshot::build` priced it once; the fold now pools each call's own
/// cache-write split (`routing::PooledUsage`) and the snapshot prices the pot.
/// On ordinary traffic (`held_usd == 0`, no failed settle, no mid-settle
/// restart — none of which this test constructs) the two must agree, or
/// `drift_usd` reports "money lost" for a pricing-function artifact.
#[test]
fn the_rollup_prices_two_turns_exactly_as_settling_them_one_at_a_time_does() {
    let a = call(1_000, 0);
    let b = call(2_000, 2_000);

    // What turn-by-turn settlement commits: each call priced on its own, the
    // dollars summed.
    let committed_equivalent = CLAUDE.price(&a) + CLAUDE.price(&b);

    // What the metrics rollup actually publishes as `frontier_spend_usd`,
    // i.e. what `admin_api::reconciliation` reads into `measured_usd`.
    let snapshot = snapshot_of(&[a, b]);
    let measured_equivalent = snapshot.savings.frontier_spend_usd;

    assert!(
        (committed_equivalent - measured_equivalent).abs() < 1e-9,
        "turn-by-turn settlement would commit ${committed_equivalent:.6} for these two \
         calls, but the rollup that feeds reconciliation's measured_usd reports \
         ${measured_equivalent:.6} -- a ${:.6} gap on ordinary traffic with no failed \
         settle, no mid-settle restart, and nothing held",
        committed_equivalent - measured_equivalent,
    );

    // Same figure one level down, where `ModelMetrics::billed_usd()` is the
    // per-row cut of the same headline.
    let row = &snapshot.models[0];
    assert!(
        (row.billed_usd() - committed_equivalent).abs() < 1e-9,
        "row.billed_usd() = {} but turn-by-turn settlement = {committed_equivalent}",
        row.billed_usd(),
    );
}

/// CONTROL, live throughout — it passed while the two tests above were failing
/// evidence and it passes now. On a *homogeneous* pot (both calls agreeing on
/// whether a write was measured) even the summed-`Usage` technique reports no
/// gap, because each call stays on the same branch of `price()` the whole way
/// through and that branch alone is linear. This is what proved the two
/// failures were about a *mixed* pot specifically, and not floating-point noise
/// or a bug in this file's arithmetic: change one input (give turn B no
/// measured write) and the assertion they failed on passed exactly.
#[test]
fn price_is_additive_when_neither_turn_in_the_pot_measured_a_write() {
    let c = call(1_000, 0);
    let d = call(2_000, 0);

    let sum_of_prices = CLAUDE.price(&c) + CLAUDE.price(&d);
    let mut summed = c.clone();
    summed.add(&d);
    let price_of_sum = CLAUDE.price(&summed);
    assert!(
        (sum_of_prices - price_of_sum).abs() < 1e-9,
        "price(c) + price(d) = ${sum_of_prices:.6} but price(c + d) = ${price_of_sum:.6} -- \
         these should agree exactly when both calls take the unmeasured branch"
    );

    let snapshot = snapshot_of(&[c, d]);
    assert!(
        (snapshot.savings.frontier_spend_usd - sum_of_prices).abs() < 1e-9,
        "the rollup ({}) should match turn-by-turn settlement ({sum_of_prices}) on a \
         homogeneous pot",
        snapshot.savings.frontier_spend_usd,
    );
}
