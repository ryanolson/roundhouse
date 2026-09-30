// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The promotion gates' inputs, one rule per test (design in
//! `docs/src/concepts/routing-learner.md`): which recorded number each
//! gate reads, which guard answers first, what each counter counts, and which
//! interval is excluded before any weight can stop being a number.
//!
//! The staged rulings themselves are in `learner_offline_promotion.rs`.

mod learning_support;

use learning_support::*;
use roundhouse_core::control::ProjectId;
use roundhouse_core::event::{Accounting, Usage};
use roundhouse_core::routing::ProviderPricing;
use roundhouse_core::routing::learn::offline::estimate::paired_bootstrap;
use roundhouse_core::routing::learn::offline::{
    ArtifactPrior, BootstrapPlan, Calibrated, CalibrationConfig, Candidate, Cause, CostEstimate,
    DriftCheck, Evidence, IntervalFacts, Money, Outcome, QualityMinimum, QuoteCensus, SessionLog,
    Source, TestResult, assemble, corrected_cost, corrected_first_output, paired_quality,
};
use roundhouse_core::routing::learn::{
    CostCorrection, CostEvidence, Draw, ExplorationEvidence, LatencyTerm, LearnedChoice,
    OnInfeasible, Strategy, StrategySet, TtftEvidence,
};
use roundhouse_core::session::MAX_REVIEW_TURNS;

fn config(min_sessions: u64) -> CalibrationConfig {
    CalibrationConfig {
        project: ProjectId::new("acme"),
        strategies: StrategySet::new(vec![
            Strategy::Rules,
            Strategy::Efficient,
            Strategy::Capable,
        ])
        .unwrap(),
        prior: ArtifactPrior::Credit,
        quality: QualityMinimum { min_sessions },
        latency_limit_ms: 10_000,
        bootstrap: BootstrapPlan {
            seed: 11,
            resamples: 200,
        },
        cutoff: None,
    }
}

fn run(config: &CalibrationConfig, scripts: &[Script]) -> Calibrated {
    let mut logs: Vec<SessionLog> = scripts
        .iter()
        .map(|script| SessionLog {
            session: script.session.clone(),
            events: script.events.clone(),
        })
        .collect();
    logs.sort_by(|a, b| a.session.cmp(&b.session));
    let source = Source::from_logs(config.project.clone(), logs);
    let evidence = Evidence::extract(config, &source.logs);
    assemble(config, source, evidence, DriftCheck::NotRun, "test-commit").unwrap()
}

const CARD: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 10.0,
    cached_input_per_mtok_usd: 1.0,
    cache_write_per_mtok_usd: 10.0,
    output_per_mtok_usd: 0.0,
};

/// One session of one reviewed `shadow` turn, served on `opus`.
fn session(id: &str, spec: Spec, positive: bool) -> Script {
    let mut script = Script::named(id);
    let mut turn = script.begin();
    script.route(&mut turn, spec.rate_card(CARD).decision());
    let at = script.clock + 10;
    script.delta_at(&turn, at, "answer");
    script.complete(
        &turn,
        Usage {
            input_tokens: 1_000,
            output_tokens: 0,
            accounting: Accounting::Reported,
            ..Usage::default()
        },
    );
    script.review(&[&turn], if positive { on_track() } else { off_track() });
    script
}

const APPLIED: LatencyTerm = LatencyTerm::Applied { mean_ms: 0 };

fn cost(quoted_usd: f64, adjusted_usd: f64, correction: CostCorrection) -> CostEvidence {
    CostEvidence {
        quoted_usd,
        adjusted_usd,
        correction,
    }
}

fn ttft(ms: f64, residual: LatencyTerm, overhead: LatencyTerm) -> TtftEvidence {
    TtftEvidence {
        quoted_ms: ms,
        adjusted_ms: ms,
        residual,
        overhead,
    }
}

/// An agreeing `shadow` session (nothing passes, so the learned choice is
/// `rules`) whose `rules` plan recorded `rules_cost` and `rules_ttft`.
fn agreeing_quoted(id: &str, rules_cost: CostEvidence, rules_ttft: TtftEvidence) -> Script {
    let spec = Spec::new()
        .corrected(Strategy::Efficient, 0.002, 600.0)
        .corrected(Strategy::Capable, 0.01, 800.0)
        .quote(Strategy::Rules, rules_cost, rules_ttft);
    session(id, spec, true)
}

fn agreeing(id: &str) -> Script {
    agreeing_quoted(
        id,
        cost(0.01, 0.01, CostCorrection::Applied),
        ttft(800.0, APPLIED, APPLIED),
    )
}

/// `efficient` passes, so the learned choice is `haiku` at $0.02; `shadow`
/// served `rules` on `opus` at $0.10.
fn diverging(id: &str, positive: bool) -> Script {
    let spec = Spec::new()
        .passing(Strategy::Efficient, 0.02)
        .choice(LearnedChoice::Exploit {
            strategy: Strategy::Efficient,
        })
        .corrected(Strategy::Rules, 0.10, 800.0)
        .corrected(Strategy::Efficient, 0.02, 600.0)
        .corrected(Strategy::Capable, 0.10, 800.0);
    session(id, spec, positive)
}

/// Five agreeing sessions and five diverging ones, four of those positive.
fn refuter_fixture() -> Vec<Script> {
    let mut scripts: Vec<Script> = (0..5)
        .map(|at| agreeing(&format!("acme/ada/agree{at}#g0")))
        .collect();
    scripts.extend((0..5).map(|at| diverging(&format!("acme/ada/diverge{at}#g0"), at != 0)));
    scripts
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line with `{needle}` in:\n{text}"))
}

fn exploration(rate: f64, set: Vec<Strategy>) -> ExplorationEvidence {
    ExplorationEvidence {
        on_infeasible: Some(OnInfeasible::ServeRules),
        draw: Draw { rate, member: 0 },
        possible: true,
        set,
    }
}

/// A `live` turn where nothing passes, so the learned choice is `rules` on
/// `opus`; explored, it served `efficient` on `haiku`, and neither side has
/// weight. `efficient` is priced below `rules`, so the set is
/// `[efficient, rules]`.
fn agreeing_live(explored: bool) -> Spec {
    let set = vec![Strategy::Efficient, Strategy::Rules];
    let spec = Spec::new()
        .live()
        .priced(Strategy::Efficient, 0.005)
        .propensity(0.5);
    if explored {
        spec.chosen(haiku())
            .choice(LearnedChoice::Explore {
                strategy: Strategy::Efficient,
                member: 0,
            })
            .exploration(exploration(0.0, set))
    } else {
        spec.chosen(opus()).exploration(exploration(0.9, set))
    }
}

/// A `live` turn where `efficient` passes, so the learned choice is `haiku`.
/// Served, only learned has weight. Explored to `rules` on `opus`, only
/// `rules` has weight. `rules` and `capable` both cost less than the $0.02
/// exploit, so the set is `[rules, capable]`.
fn diverging_live(explored: bool) -> Spec {
    let set = vec![Strategy::Rules, Strategy::Capable];
    let spec = Spec::new().live().passing(Strategy::Efficient, 0.02);
    if explored {
        spec.chosen(opus())
            .propensity(0.05)
            .choice(LearnedChoice::Explore {
                strategy: Strategy::Rules,
                member: 0,
            })
            .exploration(exploration(0.0, set))
    } else {
        spec.chosen(haiku())
            .propensity(0.95)
            .choice(LearnedChoice::Exploit {
                strategy: Strategy::Efficient,
            })
            .exploration(exploration(0.9, set))
    }
}

/// One session of one-turn intervals, each `(spec, positive)`.
fn live_session(id: &str, intervals: &[(Spec, bool)]) -> Script {
    let mut script = Script::named(id);
    for (spec, positive) in intervals {
        let turn = script.turn(spec.decision());
        script.review(&[&turn], if *positive { on_track() } else { off_track() });
    }
    script
}

fn reason(result: TestResult) -> &'static str {
    match result {
        TestResult::NotEvaluable(reason) => reason,
        other => panic!("expected not evaluable, got {other:?}"),
    }
}

fn plan(resamples: u32) -> BootstrapPlan {
    BootstrapPlan { seed: 5, resamples }
}

/// The corrected cost is the larger of the corrected and the quoted figure,
/// and that is the price the cost test compares. Every other fixture records
/// the two equal, so neither half of the `max` was held.
#[test]
fn the_cost_test_reads_the_larger_of_the_corrected_and_the_quoted_cost() {
    let latency = ttft(800.0, APPLIED, APPLIED);
    let quote_above = cost(0.03, 0.01, CostCorrection::Applied);
    let corrected_above = cost(0.01, 0.04, CostCorrection::Applied);
    assert_eq!(corrected_cost(&quote_above), Money::Priced(0.03));
    assert_eq!(corrected_cost(&corrected_above), Money::Priced(0.04));
    for (evidence, price) in [(quote_above, 0.03), (corrected_above, 0.04)] {
        let calibrated = run(
            &config(1),
            &[agreeing_quoted("acme/ada/one#g0", evidence, latency)],
        );
        let test = calibrated.report.promotion.cost;
        assert_eq!(test.rules, CostEstimate::Priced(price), "{evidence:?}");
        assert_eq!(test.learned, CostEstimate::Priced(price), "{evidence:?}");
    }
}

/// A `NoPredictedReuse` quote is priced, at the uncached bound the correction recorded
/// in `adjusted_usd`, and the census says so.
#[test]
fn a_no_predicted_reuse_quote_is_priced_at_its_uncached_bound() {
    let calibrated = run(
        &config(1),
        &[agreeing_quoted(
            "acme/ada/one#g0",
            cost(0.01, 0.025, CostCorrection::NoPredictedReuse),
            ttft(800.0, APPLIED, APPLIED),
        )],
    );
    let promotion = &calibrated.report.promotion;
    assert_eq!(promotion.cost.rules, CostEstimate::Priced(0.025));
    assert_eq!(promotion.cost.uncorrectable, 0);
    assert_eq!(promotion.cost.result, TestResult::Fail, "no saving at all");
    let text = calibrated.report.render();
    let census = line_with(&text, "cost quotes read");
    assert!(
        census.contains("1 with no predicted reuse (priced without the cache discount)"),
        "{census}"
    );
}

/// A modeled first output needs both M3 latency terms. Either one alone
/// leaves the turn uncorrectable, and the test not evaluable.
#[test]
fn a_first_output_with_one_latency_term_applied_is_uncorrectable() {
    let priced = cost(0.01, 0.01, CostCorrection::Applied);
    for (residual, overhead) in [
        (APPLIED, LatencyTerm::TooFewSamples),
        (LatencyTerm::TooFewSamples, APPLIED),
    ] {
        let calibrated = run(
            &config(1),
            &[agreeing_quoted(
                "acme/ada/one#g0",
                priced,
                ttft(800.0, residual, overhead),
            )],
        );
        let latency = calibrated.report.promotion.latency;
        assert_eq!(
            (latency.uncorrectable, latency.turns, latency.p50_ms),
            (1, 1, None),
            "{residual:?} {overhead:?}"
        );
        assert!(matches!(latency.result, TestResult::NotEvaluable(_)));
    }
    assert_eq!(
        corrected_first_output(&ttft(800.4, APPLIED, APPLIED)),
        Some(801)
    );
}

/// The paired difference is learned minus `rules`. On the agreeing set the
/// order cannot show, so the test feeds `paired_quality` diverging `live`
/// intervals: learned has weight only on its served negatives, `rules` only
/// on the explored positives. Learned minus `rules` is -1 on every resample.
#[test]
fn the_paired_difference_is_learned_minus_rules() {
    let scripts: Vec<Script> = (0..20)
        .map(|at| {
            live_session(
                &format!("acme/ada/live{at:02}#g0"),
                &[(diverging_live(false), false), (diverging_live(true), true)],
            )
        })
        .collect();
    let calibrated = run(&config(20), &scripts);
    assert_eq!(calibrated.evidence.intervals.len(), 40);
    assert_eq!(calibrated.report.promotion.agreeing, 0);
    let intervals: Vec<&IntervalFacts> = calibrated.evidence.intervals.iter().collect();
    let test = paired_quality(&intervals, plan(200), 20);
    assert_eq!(test.sessions, 20);
    assert_eq!(test.lower, Some(-1.0));
    assert_eq!(test.result, TestResult::Fail);
}

/// Each reason the paired test cannot be evaluated has its own message, and
/// the guards answer in order: an empty set, too few sessions, no learned
/// weight, no `rules` weight, sparse support.
#[test]
fn each_paired_not_evaluable_reason_has_its_own_message_in_guard_order() {
    let empty = paired_quality(&[], plan(200), 20);
    assert!(reason(empty.result).contains("no interval in the set"));

    // Every interval explored away: neither side has weight, so the real
    // session count is 0. Against `min_sessions` 20 that is `few` first; at
    // `min_sessions` 0 the guard passes and `learned` is reached honestly,
    // on the same fixture.
    let unweighted: Vec<Script> = (0..20)
        .map(|at| {
            live_session(
                &format!("acme/ada/live{at:02}#g0"),
                &[(agreeing_live(true), true)],
            )
        })
        .collect();
    let calibrated = run(&config(20), &unweighted);
    let intervals: Vec<&IntervalFacts> = calibrated.evidence.intervals.iter().collect();
    let few = reason(paired_quality(&intervals, plan(200), 20).result);
    assert!(few.contains("quality.min_sessions"), "{few}");
    let learned = reason(paired_quality(&intervals, plan(200), 0).result);
    assert!(learned.contains("learned has no interval"), "{learned}");

    // Served diverging turns: learned has weight on all 20 real sessions,
    // `rules` has none.
    let served: Vec<Script> = (0..20)
        .map(|at| {
            live_session(
                &format!("acme/ada/live{at:02}#g0"),
                &[(diverging_live(false), true)],
            )
        })
        .collect();
    let calibrated = run(&config(20), &served);
    let intervals: Vec<&IntervalFacts> = calibrated.evidence.intervals.iter().collect();
    let rules = reason(paired_quality(&intervals, plan(200), 20).result);
    assert!(rules.contains("rules has no interval"), "{rules}");

    // Three of fifty sessions hold weight, the real count `paired_quality`
    // now computes itself: `min_sessions` 3 matches it exactly, so the guard
    // passes and the bootstrap's own sparsity is what fails the test.
    let sparse: Vec<Script> = (0..50)
        .map(|at| {
            live_session(
                &format!("acme/ada/live{at:02}#g0"),
                &[(agreeing_live(at >= 3), true)],
            )
        })
        .collect();
    let calibrated = run(&config(20), &sparse);
    let intervals: Vec<&IntervalFacts> = calibrated.evidence.intervals.iter().collect();
    let test = paired_quality(&intervals, plan(200), 3);
    assert_eq!(
        test.sessions, 3,
        "the real weighted session count, not a claimed one"
    );
    let filler = reason(test.result);
    assert!(filler.contains("filler"), "{filler}");

    let reasons = [few, learned, rules, filler];
    for (at, one) in reasons.iter().enumerate() {
        for other in &reasons[at + 1..] {
            assert_ne!(one, other);
        }
    }
    for (phrase, owner) in [
        ("quality.min_sessions", few),
        ("learned has no interval", learned),
        ("rules has no interval", rules),
        ("filler", filler),
    ] {
        for held in reasons.iter().filter(|held| **held != owner) {
            assert!(!held.contains(phrase), "`{held}` contains `{phrase}`");
        }
    }
}

/// An undefined paired replicate counts as -1 for the lower bound and +1 for
/// the upper: the worst each bound of a difference of two rates can be.
#[test]
fn an_undefined_paired_replicate_widens_the_bound_to_minus_one_and_one() {
    let outcome = |cluster: usize, positive: bool, weight: f64| Outcome {
        cluster,
        turns: 1,
        positive,
        weight,
        supported: weight > 0.0,
        matched: weight > 0.0,
        cost: Money::Priced(0.01),
        first_output_ms: Vec::new(),
    };
    let empty: Vec<Outcome> = (0..4).map(|cluster| outcome(cluster, true, 0.0)).collect();
    let weighted: Vec<Outcome> = (0..4).map(|cluster| outcome(cluster, true, 1.0)).collect();
    for (learned, rules) in [(&empty, &empty), (&weighted, &empty), (&empty, &weighted)] {
        let bounds = paired_bootstrap(learned, rules, plan(40));
        assert_eq!((bounds.lower, bounds.upper), (Some(-1.0), Some(1.0)));
        assert_eq!(bounds.undefined, 40);
        assert!(bounds.sparse);
    }
}

/// The staged verdict needs latency too: cost and agreeing quality pass, the
/// learned plans' p50 of 600 ms is over a 500 ms limit, and nothing promotes.
#[test]
fn a_latency_failure_alone_blocks_promotion() {
    let mut config = config(2);
    config.latency_limit_ms = 500;
    let calibrated = run(&config, &refuter_fixture());
    let promotion = &calibrated.report.promotion;
    assert_eq!(promotion.cost.result, TestResult::Pass);
    assert_eq!(promotion.quality_agreeing.result, TestResult::Pass);
    assert_eq!(promotion.latency.p50_ms, Some(600));
    assert_eq!(promotion.latency.result, TestResult::Fail);
    assert!(!promotion.promotable());
    let text = calibrated.report.render();
    assert!(line_with(&text, "promotion to live").contains(": no"));
}

/// The census counts each plan the cost test reads once: the learned plan,
/// and the `rules` plan when the learned one is another strategy's.
#[test]
fn the_quote_census_counts_each_plan_the_cost_test_reads_once() {
    let calibrated = run(&config(2), &refuter_fixture());
    assert_eq!(
        calibrated.report.promotion.quotes,
        QuoteCensus {
            applied: 15,
            ..QuoteCensus::default()
        }
    );

    let latency = ttft(800.0, APPLIED, APPLIED);
    let mut scripts = refuter_fixture();
    for (id, correction) in [
        ("acme/ada/npr#g0", CostCorrection::NoPredictedReuse),
        ("acme/ada/cold#g0", CostCorrection::TooFewSamples),
        ("acme/ada/local#g0", CostCorrection::NotFrontier),
    ] {
        scripts.push(agreeing_quoted(id, cost(0.01, 0.01, correction), latency));
    }
    let calibrated = run(&config(2), &scripts);
    assert_eq!(
        calibrated.report.promotion.quotes,
        QuoteCensus {
            applied: 15,
            no_predicted_reuse: 1,
            too_few_samples: 1,
            local: 1,
            missing: 0,
        }
    );
}

/// `quality.min_sessions` counts the sessions whose agreeing intervals give
/// learned weight. Sixteen `live` sessions whose only agreeing interval
/// explored away carry none, so four sessions stand against twenty.
#[test]
fn min_sessions_counts_only_agreeing_sessions_where_learned_has_weight() {
    let mut scripts: Vec<Script> = (0..16)
        .map(|at| {
            live_session(
                &format!("acme/ada/explored{at:02}#g0"),
                &[(agreeing_live(true), true)],
            )
        })
        .collect();
    scripts.extend((0..4).map(|at| {
        live_session(
            &format!("acme/ada/served{at}#g0"),
            &[(agreeing_live(false), true)],
        )
    }));
    let calibrated = run(&config(20), &scripts);
    let promotion = &calibrated.report.promotion;
    assert_eq!(promotion.agreeing, 20);
    assert_eq!(promotion.agreeing_sessions, 4);
    let text = calibrated.report.render();
    let line = line_with(&text, "quality.min_sessions 20");
    assert!(line.ends_with(": not met"), "{line}");
    assert!(
        reason(promotion.quality_agreeing.result).contains("quality.min_sessions"),
        "{:?}",
        promotion.quality_agreeing
    );
}

/// 64 matched `live` turns at propensity 1e-5: the product is 1e-320,
/// subnormal or zero by platform, and its inverse overflows. The interval is
/// excluded under its own cause, and every estimate stays a number.
///
/// 64 is the most turns one review covers (`MAX_REVIEW_TURNS`); a review of
/// 200 turns is never accepted, so no product of the ruled 0.025 can
/// underflow in one interval, and the propensity is chosen to make it.
#[test]
fn a_trajectory_weight_that_is_not_finite_is_excluded_by_its_cause() {
    let mut long = Script::named("acme/ada/long#g0");
    let turns: Vec<Turn> = (0..MAX_REVIEW_TURNS)
        .map(|_| {
            long.turn(
                Spec::new()
                    .live()
                    .propensity(1e-5)
                    .exploration(exploration(0.9, vec![Strategy::Rules]))
                    .decision(),
            )
        })
        .collect();
    long.review(&turns.iter().collect::<Vec<_>>(), on_track());
    let mut scripts = vec![long];
    scripts.extend((0..4).map(|at| agreeing(&format!("acme/ada/agree{at}#g0"))));
    let calibrated = run(&config(1), &scripts);
    assert_eq!(calibrated.evidence.accepted_reviews, 5);
    for (candidate, estimate) in &calibrated.report.candidates {
        if *candidate == Candidate::Fixed(Strategy::Efficient) {
            continue;
        }
        let bounds = estimate.bootstrap;
        for value in [estimate.snips, bounds.lower, bounds.upper] {
            assert!(
                value.is_some_and(f64::is_finite),
                "{candidate:?}: {estimate:?}"
            );
        }
    }
    assert_eq!(
        calibrated.evidence.exclusions.get(&Cause::NonFiniteWeight),
        Some(&1)
    );
    assert_eq!(calibrated.evidence.intervals.len(), 4);
    let text = calibrated.report.render();
    assert_eq!(
        line_with(&text, "- trajectory weight not finite:"),
        "- trajectory weight not finite: 1"
    );
}

/// The manifest refuses a bootstrap with fewer than 40 resamples, the
/// fewest that give the 2.5% tail a place, and names the field.
#[test]
fn a_calibration_config_with_fewer_than_forty_resamples_is_refused() {
    let json = |resamples: u32| {
        format!(
            r#"{{"project": "acme", "strategies": ["rules", "capable"], "prior": "zero",
                "quality": {{"min_sessions": 20}}, "latency_limit_ms": 10000,
                "bootstrap": {{"seed": 1, "resamples": {resamples}}}}}"#
        )
    };
    let error = serde_json::from_str::<CalibrationConfig>(&json(39)).unwrap_err();
    assert!(error.to_string().contains("resamples"), "{error}");
    let config: CalibrationConfig = serde_json::from_str(&json(40)).unwrap();
    assert_eq!(config.bootstrap.resamples, 40);
}
