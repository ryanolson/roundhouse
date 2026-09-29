// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner's cost, latency and grant corrections (plan M3).
//!
//! Three claims. The cache correction re-prices a reuse shortfall through the
//! one pricing contract, so the write premium survives, and it never prices a
//! route below its quote (the 2026-09-28 owner rule). The latency model is the
//! quote plus the target's residual plus one project overhead, each term only
//! once it has its minimum samples (ruling 10). And the grant is checked again
//! on the corrected cost, except for a candidate the overflow valve admitted.

use roundhouse_core::control::{BudgetState, Exhaustion, TurnBudget};
use roundhouse_core::routing::learn::{
    CacheReuse, Corrections, CostCorrection, CostEvidence, GrantCheck, LatencySum, LatencyTerm,
    ReadView, TargetOps, TtftEvidence, adjusted_cached_tokens, grant, latency_term,
};
use roundhouse_core::routing::{
    CacheLedger, CacheModel, Candidate, LocalCapacityPrice, ProviderPricing, Target,
};

const MTOK: usize = 1_000_000;

/// The draft's parent rate card: input 1, write 2, read 0.1.
const PARENT: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 1.0,
    cached_input_per_mtok_usd: 0.1,
    cache_write_per_mtok_usd: 2.0,
    output_per_mtok_usd: 0.0,
};

fn sol() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "sol".into(),
    }
}

fn luna() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "luna".into(),
    }
}

fn worker() -> Target {
    Target::Local {
        worker_id: 7,
        dp_rank: 0,
        model: "llama".into(),
    }
}

fn ledger() -> CacheLedger {
    let mut ledger = CacheLedger::new();
    for target in [sol(), luna()] {
        ledger.register(
            &target,
            CacheModel::Deterministic { ttl_ms: 300_000 },
            PARENT,
        );
    }
    ledger
}

/// A frontier candidate shaped as `FrontierCatalog::quote` shapes one: the
/// matched prefix is the floor of the expected cached count.
fn frontier(target: Target, isl: usize, cached: f64, ttft_ms: f64) -> Candidate {
    let uncached = isl as f64 - cached;
    Candidate {
        target,
        expected_prefill_tokens: uncached,
        matched_prefix_tokens: cached as u64,
        expected_ttft_ms: ttft_ms,
        expected_cost_usd: PARENT.price_tokens(uncached, cached, 0.0),
        quality_prior: 0.9,
        load: None,
    }
}

fn ops(target: &Target, latency: LatencySum, cache: CacheReuse) -> TargetOps {
    TargetOps {
        target: target.policy_identity(),
        latency,
        failover: 0,
        cache,
    }
}

fn view(targets: Vec<TargetOps>, overhead: LatencySum) -> ReadView {
    ReadView {
        levels: Vec::new(),
        targets,
        overhead,
    }
}

fn reuse(predicted_permille: u64, observed_permille: u64, n: u64) -> CacheReuse {
    CacheReuse {
        predicted_permille,
        observed_permille,
        n,
    }
}

fn samples(sum_ms: i64, n: u64) -> LatencySum {
    LatencySum { sum_ms, n }
}

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

const MIN: u64 = 20;

/// **The parent case: 100 becomes 200.** A predicted-warm 100 MTok prefix
/// that the target's history says is never reused is re-priced as uncached,
/// and uncached prompt tokens pay the write rate (2), not the plain input rate
/// (1). Revision 1 of the draft used input minus read and got 100.
#[test]
fn a_reuse_shortfall_keeps_the_effective_write_premium() {
    let isl = 100 * MTOK;
    let candidate = frontier(sol(), isl, isl as f64, 800.0);
    assert!(
        close(candidate.expected_cost_usd, 10.0),
        "quote is 100 reads"
    );
    let view = view(
        vec![ops(
            &sol(),
            LatencySum::default(),
            reuse(1_000 * MIN, 0, MIN),
        )],
        LatencySum::default(),
    );
    let ledger = ledger();
    let corrections = Corrections::new(&view, &ledger, isl, MIN, MIN);

    let cost = corrections.cost(&candidate);

    assert_eq!(cost.correction, CostCorrection::Applied);
    assert!(close(cost.quoted_usd, 10.0));
    assert!(
        close(cost.adjusted_usd, 200.0),
        "100 MTok moved to uncached must bill at the write rate: got {}",
        cost.adjusted_usd
    );
}

/// No predicted reuse means no ratio, so nothing moves and the record says why.
/// A local target is never corrected either, even now that a configured
/// capacity price gives it a nonzero quote (PR 23).
#[test]
fn zero_predicted_reuse_applies_no_correction() {
    let isl = MTOK;
    let candidate = frontier(sol(), isl, 0.0, 800.0);
    let view = view(
        vec![
            ops(&sol(), LatencySum::default(), reuse(0, 0, MIN)),
            ops(&luna(), LatencySum::default(), reuse(500, 100, MIN - 1)),
        ],
        LatencySum::default(),
    );
    let ledger = ledger();
    let corrections = Corrections::new(&view, &ledger, isl, MIN, MIN);

    let cost = corrections.cost(&candidate);
    assert_eq!(cost.correction, CostCorrection::NoPredictedReuse);
    assert_eq!(cost.adjusted_usd, cost.quoted_usd);

    let thin = corrections.cost(&frontier(luna(), isl, 500_000.0, 800.0));
    assert_eq!(thin.correction, CostCorrection::TooFewSamples);
    assert_eq!(thin.adjusted_usd, thin.quoted_usd);

    let price = LocalCapacityPrice {
        input_per_mtok_usd: 0.5,
        output_per_mtok_usd: 1.5,
    };
    let local = Candidate {
        target: worker(),
        expected_prefill_tokens: 400_000.0,
        matched_prefix_tokens: 600_000,
        expected_ttft_ms: 50.0,
        expected_cost_usd: price.price_tokens(400_000.0, 1_000.0),
        quality_prior: 0.6,
        load: Some(0.0),
    };
    let view = view_with_local_shortfall();
    let corrections = Corrections::new(&view, &ledger, isl, MIN, MIN);
    let cost = corrections.cost(&local);
    assert_eq!(cost.correction, CostCorrection::NotFrontier);
    assert!(cost.quoted_usd > 0.0, "a priced local quote is not $0");
    assert_eq!(cost.adjusted_usd, cost.quoted_usd);
}

fn view_with_local_shortfall() -> ReadView {
    view(
        vec![ops(
            &worker(),
            LatencySum::default(),
            reuse(900 * MIN, 0, MIN),
        )],
        LatencySum::default(),
    )
}

/// The corrected reused count is bounded by the matched prefix, the input and
/// the quote's own cached count, and scales by observed over predicted.
#[test]
fn the_adjusted_cached_count_stays_within_the_prefix_and_input() {
    let isl = 1_000;
    let half = reuse(1_000, 500, MIN);

    // 800 quoted, half reused: 400, under every bound.
    let wide = Candidate {
        matched_prefix_tokens: 800,
        ..frontier(sol(), isl, 800.0, 800.0)
    };
    assert_eq!(adjusted_cached_tokens(&wide, isl, &half), 400.0);

    // A matched prefix of 300 caps the 400.
    let narrow = Candidate {
        matched_prefix_tokens: 300,
        ..frontier(sol(), isl, 800.0, 800.0)
    };
    assert_eq!(adjusted_cached_tokens(&narrow, isl, &half), 300.0);

    // A malformed quote that claims more cached tokens than the input is
    // clamped to the input before the ratio applies.
    let over = Candidate {
        expected_prefill_tokens: -500.0,
        matched_prefix_tokens: 5_000,
        ..frontier(sol(), isl, 0.0, 800.0)
    };
    let full = reuse(1_000, 1_000, MIN);
    assert_eq!(adjusted_cached_tokens(&over, isl, &full), isl as f64);

    // Fractional quotes floor, and never go negative.
    let fractional = Candidate {
        matched_prefix_tokens: 999,
        ..frontier(sol(), isl, 333.7, 800.0)
    };
    assert_eq!(adjusted_cached_tokens(&fractional, isl, &half), 166.0);
    assert_eq!(
        adjusted_cached_tokens(&fractional, isl, &reuse(1_000, 0, MIN)),
        0.0
    );
}

/// **A quote can be approximate, but never cheaper than it is** (the
/// 2026-09-28 owner rule). A target that reused more than predicted does not
/// earn a quote below the ledger's own, even from a producer whose matched
/// prefix exceeds the weighted cached count.
#[test]
fn a_reuse_surplus_never_prices_a_route_below_its_quote() {
    let isl = 100 * MTOK;
    let candidate = Candidate {
        matched_prefix_tokens: isl as u64,
        ..frontier(sol(), isl, 40.0 * MTOK as f64, 800.0)
    };
    let surplus = reuse(1_000 * MIN, 2_000 * MIN, MIN);
    let view = view(
        vec![ops(&sol(), LatencySum::default(), surplus)],
        LatencySum::default(),
    );
    let ledger = ledger();
    let corrections = Corrections::new(&view, &ledger, isl, MIN, MIN);

    let cost = corrections.cost(&candidate);

    assert_eq!(cost.correction, CostCorrection::Applied);
    assert!(
        cost.adjusted_usd >= cost.quoted_usd,
        "a surplus lowered the quote from {} to {}",
        cost.quoted_usd,
        cost.adjusted_usd
    );
    assert_eq!(
        adjusted_cached_tokens(&candidate, isl, &surplus),
        40.0 * MTOK as f64
    );
}

/// The shortfall's cost appears once, inside the re-priced tokens: a full miss
/// costs exactly the write-rate price of the moved tokens less their read
/// price, and a calibrated target (observed equals predicted) costs its quote.
#[test]
fn a_cache_miss_adds_no_separate_penalty() {
    let isl = 10 * MTOK;
    let cached = 6.0 * MTOK as f64;
    let candidate = frontier(sol(), isl, cached, 800.0);
    let ledger = ledger();

    let miss = view(
        vec![ops(&sol(), LatencySum::default(), reuse(800 * MIN, 0, MIN))],
        LatencySum::default(),
    );
    let cost = Corrections::new(&miss, &ledger, isl, MIN, MIN).cost(&candidate);
    let moved = PARENT.price_tokens(cached, 0.0, 0.0) - PARENT.price_tokens(0.0, cached, 0.0);
    assert_eq!(cost.correction, CostCorrection::Applied);
    assert!(
        close(cost.adjusted_usd, cost.quoted_usd + moved),
        "adjusted {} is not the quote {} plus the re-priced tokens {moved}",
        cost.adjusted_usd,
        cost.quoted_usd
    );

    let calibrated = view(
        vec![ops(
            &sol(),
            LatencySum::default(),
            reuse(800 * MIN, 800 * MIN, MIN),
        )],
        LatencySum::default(),
    );
    let cost = Corrections::new(&calibrated, &ledger, isl, MIN, MIN).cost(&candidate);
    assert_eq!(cost.correction, CostCorrection::Applied);
    assert!(close(cost.adjusted_usd, cost.quoted_usd));

    // A cold quote (for example one whose last request placed no marker, so
    // the ledger predicts nothing warm) has nothing to move.
    let cold = frontier(sol(), isl, 0.0, 800.0);
    let cost = Corrections::new(&miss, &ledger, isl, MIN, MIN).cost(&cold);
    assert_eq!(cost.correction, CostCorrection::Applied);
    assert_eq!(cost.adjusted_usd, cost.quoted_usd);
}

/// The residual already carries the latency effect of lower reuse, so a
/// shortfall moves the cost and leaves the first-output estimate alone.
#[test]
fn the_cache_correction_does_not_change_ttft() {
    let isl = 10 * MTOK;
    let candidate = frontier(sol(), isl, 6.0 * MTOK as f64, 800.0);
    let ledger = ledger();
    let latency = samples(100 * MIN as i64, MIN);
    let overhead = samples(40 * MIN as i64, MIN);

    let calibrated = view(
        vec![ops(&sol(), latency, reuse(800 * MIN, 800 * MIN, MIN))],
        overhead,
    );
    let miss = view(
        vec![ops(&sol(), latency, reuse(800 * MIN, 0, MIN))],
        overhead,
    );

    let warm = Corrections::new(&calibrated, &ledger, isl, MIN, MIN).first_output(&candidate);
    let cold = Corrections::new(&miss, &ledger, isl, MIN, MIN).first_output(&candidate);

    assert_eq!(warm, cold);
    assert_eq!(cold.adjusted_ms, 800.0 + 100.0 + 40.0);
}

/// A residual is added only at or above its minimum count; below it the
/// estimate is the quote and the record says the term did not apply. A mean
/// that is not whole rounds up, so the estimate never understates latency.
#[test]
fn a_latency_residual_applies_only_after_its_minimum_samples() {
    let candidate = frontier(sol(), MTOK, 0.0, 800.0);
    let ledger = ledger();

    let thin = view(
        vec![ops(
            &sol(),
            samples(300 * (MIN as i64 - 1), MIN - 1),
            CacheReuse::default(),
        )],
        LatencySum::default(),
    );
    let estimate = Corrections::new(&thin, &ledger, MTOK, MIN, MIN).first_output(&candidate);
    assert_eq!(estimate.residual, LatencyTerm::TooFewSamples);
    assert_eq!(estimate.adjusted_ms, 800.0);

    let enough = view(
        vec![ops(
            &sol(),
            samples(300 * MIN as i64 + 1, MIN),
            CacheReuse::default(),
        )],
        LatencySum::default(),
    );
    let estimate = Corrections::new(&enough, &ledger, MTOK, MIN, MIN).first_output(&candidate);
    assert_eq!(estimate.residual, LatencyTerm::Applied { mean_ms: 301 });
    assert_eq!(estimate.adjusted_ms, 800.0 + 301.0);

    // A negative residual (a target faster than its quote) is a real mean and
    // rounds up towards zero.
    assert_eq!(
        latency_term(&samples(-41, 2), 2),
        LatencyTerm::Applied { mean_ms: -20 }
    );
    // A configured minimum of zero with no samples is still no mean.
    assert_eq!(latency_term(&samples(0, 0), 0), LatencyTerm::TooFewSamples);

    // A residual larger than the quote cannot put first output before the
    // turn started: the estimate stops at zero, and the term is still recorded.
    let faster = view(
        vec![ops(
            &sol(),
            samples(-1_000 * MIN as i64, MIN),
            CacheReuse::default(),
        )],
        LatencySum::default(),
    );
    let estimate = Corrections::new(&faster, &ledger, MTOK, MIN, MIN).first_output(&candidate);
    assert_eq!(estimate.residual, LatencyTerm::Applied { mean_ms: -1_000 });
    assert_eq!(estimate.adjusted_ms, 0.0);
}

/// The overhead before dispatch is the project's, not a target's: every target
/// of the turn carries the same single term, whatever the view holds for the
/// other targets.
#[test]
fn the_overhead_term_is_added_once_per_turn_not_per_target() {
    let ledger = ledger();
    let view = view(
        vec![
            ops(
                &sol(),
                samples(100 * MIN as i64, MIN),
                CacheReuse::default(),
            ),
            ops(
                &luna(),
                samples(300 * MIN as i64, MIN),
                CacheReuse::default(),
            ),
            ops(
                &worker(),
                samples(-10 * MIN as i64, MIN),
                CacheReuse::default(),
            ),
        ],
        samples(200 * MIN as i64, MIN),
    );
    let corrections = Corrections::new(&view, &ledger, MTOK, MIN, MIN);

    let capable = corrections.first_output(&frontier(sol(), MTOK, 0.0, 800.0));
    let efficient = corrections.first_output(&frontier(luna(), MTOK, 0.0, 400.0));

    let overhead = LatencyTerm::Applied { mean_ms: 200 };
    assert_eq!(
        capable,
        TtftEvidence {
            quoted_ms: 800.0,
            adjusted_ms: 800.0 + 100.0 + 200.0,
            residual: LatencyTerm::Applied { mean_ms: 100 },
            overhead,
        }
    );
    assert_eq!(
        efficient,
        TtftEvidence {
            quoted_ms: 400.0,
            adjusted_ms: 400.0 + 300.0 + 200.0,
            residual: LatencyTerm::Applied { mean_ms: 300 },
            overhead,
        }
    );
}

/// Without enough overhead samples the estimate is the quote plus the residual,
/// and the overhead is recorded as not applied rather than as a measured zero.
#[test]
fn latency_without_overhead_samples_uses_the_quote_plus_residual_and_is_recorded_unadjusted() {
    let ledger = ledger();
    let view = view(
        vec![ops(
            &sol(),
            samples(150 * MIN as i64, MIN),
            CacheReuse::default(),
        )],
        samples(5_000, MIN - 1),
    );
    let estimate = Corrections::new(&view, &ledger, MTOK, MIN, MIN).first_output(&frontier(
        sol(),
        MTOK,
        0.0,
        800.0,
    ));

    assert_eq!(estimate.residual, LatencyTerm::Applied { mean_ms: 150 });
    assert_eq!(estimate.overhead, LatencyTerm::TooFewSamples);
    assert_eq!(estimate.adjusted_ms, 800.0 + 150.0);

    // A target the view holds nothing for has no residual either.
    let unknown = Corrections::new(&view, &ledger, MTOK, MIN, MIN).first_output(&frontier(
        luna(),
        MTOK,
        0.0,
        400.0,
    ));
    assert_eq!(unknown.residual, LatencyTerm::TooFewSamples);
    assert_eq!(unknown.adjusted_ms, 400.0);
}

fn granted(ceiling_usd: f64, state: BudgetState, overflow: bool) -> TurnBudget {
    TurnBudget::Granted {
        ceiling_usd,
        state,
        on_exhaustion: Exhaustion::DegradeToLocal {
            overflow_when_local_saturated: overflow,
        },
    }
}

fn quoted(target: Target, usd: f64) -> Candidate {
    Candidate {
        expected_cost_usd: usd,
        ..frontier(target, MTOK, 0.0, 800.0)
    }
}

/// A correction can lift a cost above the grant, and then the candidate fails
/// the grant constraint although admission accepted its quote.
#[test]
fn an_adjusted_cost_above_the_grant_fails_the_grant_constraint() {
    let budget = granted(0.010, BudgetState::Warned, false);
    let candidate = quoted(sol(), 0.008);
    assert!(budget.admits(&candidate), "admission accepted the quote");

    assert_eq!(
        grant(&budget, BudgetState::Warned, &candidate, 0.012),
        GrantCheck::Exceeds
    );
    assert_eq!(
        grant(&budget, BudgetState::Warned, &candidate, 0.009),
        GrantCheck::Admits
    );
    // The check is on the corrected cost, not the quote: a quote over the
    // grant that the correction does not reach is not re-judged by its quote.
    assert_eq!(
        grant(&budget, BudgetState::Warned, &quoted(sol(), 0.011), 0.009),
        GrantCheck::Admits
    );
    // A local candidate is the budget's to exempt, whatever its capacity price.
    let local = Candidate {
        target: worker(),
        expected_cost_usd: 0.05,
        ..quoted(sol(), 0.05)
    };
    assert_eq!(
        grant(&budget, BudgetState::Warned, &local, 0.05),
        GrantCheck::Admits
    );
}

/// A candidate the overflow valve re-admitted is past the grant by definition;
/// the learner keeps that status rather than removing it for being over a
/// ceiling of zero.
#[test]
fn an_overflow_admitted_candidate_keeps_its_status() {
    let budget = granted(0.0, BudgetState::Exhausted, true);
    assert!(budget.overflow_armed());
    let candidate = quoted(sol(), 0.40);

    assert_eq!(
        grant(&budget, BudgetState::ExhaustedOverflow, &candidate, 0.55),
        GrantCheck::Overflow
    );
    assert_eq!(
        grant(&budget, BudgetState::Exhausted, &candidate, 0.55),
        GrantCheck::Exceeds
    );
}

/// The learner calls a lower-than-predicted ratio a reuse shortfall and says
/// nothing about why. No correction record, in any variant, names eviction or
/// cache pressure.
#[test]
fn no_record_names_eviction() {
    let isl = 10 * MTOK;
    let ledger = ledger();
    let view = view(
        vec![ops(
            &sol(),
            samples(100 * MIN as i64, MIN),
            reuse(800 * MIN, 0, MIN),
        )],
        samples(40 * MIN as i64, MIN),
    );
    let corrections = Corrections::new(&view, &ledger, isl, MIN, MIN);
    let candidate = frontier(sol(), isl, 6.0 * MTOK as f64, 800.0);

    let mut written = vec![
        serde_json::to_string(&corrections.cost(&candidate)).unwrap(),
        serde_json::to_string(&corrections.first_output(&candidate)).unwrap(),
    ];
    for correction in [
        CostCorrection::Applied,
        CostCorrection::NoPredictedReuse,
        CostCorrection::TooFewSamples,
        CostCorrection::NotFrontier,
    ] {
        written.push(
            serde_json::to_string(&CostEvidence {
                quoted_usd: 1.0,
                adjusted_usd: 1.0,
                correction,
            })
            .unwrap(),
        );
    }
    for term in [
        LatencyTerm::TooFewSamples,
        LatencyTerm::Applied { mean_ms: 1 },
    ] {
        written.push(serde_json::to_string(&term).unwrap());
    }
    for check in [
        GrantCheck::Admits,
        GrantCheck::Exceeds,
        GrantCheck::Overflow,
    ] {
        written.push(serde_json::to_string(&check).unwrap());
    }

    for text in written {
        let lower = text.to_lowercase();
        for word in ["evict", "pressure", "thrash"] {
            assert!(!lower.contains(word), "`{text}` names {word}");
        }
    }
}
