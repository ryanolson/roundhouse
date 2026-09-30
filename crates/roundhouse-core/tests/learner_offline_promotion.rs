// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The staged promotion of the owner's 2026-09-29 rulings, and the gate
//! arithmetic under it (the M10 review fixes of
//! `agent-docs/PLAN-online-routing-learner.md`).
//!
//! A `shadow` project never explores, so the learned candidate has weight only
//! where it agreed with `rules`. A report that compared that subset with the
//! `rules` rate over every interval would promote by selection alone. These
//! tests hold the rulings: cost and latency from corrected quotes on every
//! eligible interval, quality only on the agreeing intervals, and `not
//! evaluable` wherever a test lacks support.

mod learning_support;

use learning_support::*;
use roundhouse_core::control::ProjectId;
use roundhouse_core::event::{Accounting, Usage};
use roundhouse_core::routing::ProviderPricing;
use roundhouse_core::routing::learn::offline::estimate::{
    BOOTSTRAP_LEVEL, bootstrap, bootstrap_replicates, estimate, paired_bootstrap, paired_replicates,
};
use roundhouse_core::routing::learn::offline::{
    ArtifactPrior, BootstrapPlan, COST_REDUCTION, Calibrated, CalibrationConfig, Candidate,
    DriftCheck, Evidence, Money, Outcome, QUALITY_ALLOWANCE, QualityMinimum, SessionLog, Source,
    TestResult, assemble, cost_gate, paired_quality_gate, quality_gate, quality_test,
};
use roundhouse_core::routing::learn::{
    Draw, ExplorationEvidence, LearnedChoice, Strategy, StrategySet,
};

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

/// Every uncached token at $10 per MTok, so 1000 input tokens are $0.01.
const CARD: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 10.0,
    cached_input_per_mtok_usd: 1.0,
    cache_write_per_mtok_usd: 10.0,
    output_per_mtok_usd: 0.0,
};

fn usage(input_tokens: u64, accounting: Accounting) -> Usage {
    Usage {
        input_tokens,
        output_tokens: 0,
        accounting,
        ..Usage::default()
    }
}

/// One session of one reviewed turn, served on `opus` at [`CARD`] for
/// `input_tokens` of reported usage.
fn session(id: &str, spec: Spec, input_tokens: u64, positive: bool) -> Script {
    let mut script = Script::named(id);
    let mut turn = script.begin();
    script.route(&mut turn, spec.rate_card(CARD).decision());
    let at = script.clock + 10;
    script.delta_at(&turn, at, "answer");
    script.complete(&turn, usage(input_tokens, Accounting::Reported));
    script.review(&[&turn], if positive { on_track() } else { off_track() });
    script
}

/// Nothing passes, so the learned choice is `rules` on `opus`; the served
/// turn costs $0.01.
fn agreeing(id: &str) -> Script {
    let spec = Spec::new()
        .corrected(Strategy::Rules, 0.01, 800.0)
        .corrected(Strategy::Efficient, 0.002, 600.0)
        .corrected(Strategy::Capable, 0.01, 800.0);
    session(id, spec, 1_000, true)
}

/// `efficient` passes, so the learned choice is `haiku`; shadow still served
/// `rules` on `opus`, and that turn costs $0.10.
fn diverging(id: &str, positive: bool) -> Script {
    let spec = Spec::new()
        .passing(Strategy::Efficient, 0.02)
        .choice(LearnedChoice::Exploit {
            strategy: Strategy::Efficient,
        })
        .corrected(Strategy::Rules, 0.10, 800.0)
        .corrected(Strategy::Efficient, 0.02, 600.0)
        .corrected(Strategy::Capable, 0.10, 800.0);
    session(id, spec, 10_000, positive)
}

/// The refuter's fixture: ten shadow sessions, five where the learned choice
/// agreed with `rules` (all positive, $0.01) and five where it did not (four
/// positive, $0.10).
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

/// M1: the learned candidate's weighted intervals are exactly the ones where
/// it agreed with `rules`, so comparing them with the `rules` rate and cost
/// over every interval passes quality (1.0 against 0.9) and cost ($0.01
/// against $0.055) by which intervals agreed, not by routing. Under the
/// staged rulings the report prices both sides from corrected quotes on all
/// ten intervals, gates quality on the five agreeing ones, and says the
/// diverging share cannot be evaluated from `shadow`.
#[test]
fn a_shadow_report_does_not_promote_by_selection_alone() {
    let calibrated = run(&config(2), &refuter_fixture());
    let text = calibrated.report.render();
    println!("{text}");
    assert!(
        !text.contains("all three ruled tests pass: yes"),
        "promoted by selection alone:\n{text}"
    );
    let diverging = line_with(&text, "quality, over every eligible interval");
    assert!(
        diverging.contains("not evaluable") && !diverging.ends_with(": pass"),
        "{diverging}"
    );
    assert!(
        line_with(
            &text,
            "quality, on the intervals where learned agreed with rules"
        )
        .contains("5 of 10 intervals differed"),
        "{text}"
    );
    let cost = line_with(&text, "2. cost:");
    assert!(cost.contains("corrected quote estimate"), "{cost}");
    // learned: 5 x $0.01 + 5 x $0.02 over 10; rules: 5 x $0.01 + 5 x $0.10.
    assert!(
        cost.contains("$0.015000") && cost.contains("$0.055000"),
        "{cost}"
    );
    assert!(cost.ends_with(": pass"), "{cost}");
    let latency = line_with(&text, "3. latency:");
    assert!(
        latency.contains("corrected quote estimate") && latency.contains("600 ms"),
        "{latency}"
    );
    assert!(latency.ends_with(": pass"), "{latency}");
    // The staged conditions hold here: cost, latency, and no loss on the
    // agreeing intervals. The verdict says so, and says what it still owes.
    let verdict = line_with(&text, "promotion to live");
    assert!(verdict.contains(": yes"), "{verdict}");
    assert!(verdict.contains("M11"), "{verdict}");
}

/// A quote the M3 terms did not correct is `unpriced`, never $0, and the
/// cost test that needs it cannot be evaluated. Dropping that interval from
/// both sides would be selection again.
#[test]
fn an_uncorrectable_quote_makes_the_cost_test_not_evaluable() {
    let mut scripts = refuter_fixture();
    // `Spec::new()` records every plan's correction as `TooFewSamples`.
    scripts.push(session("acme/ada/cold#g0", Spec::new(), 1_000, true));
    let calibrated = run(&config(2), &scripts);
    let text = calibrated.report.render();
    let cost = line_with(&text, "2. cost:");
    assert!(
        cost.contains("not evaluable") && cost.contains("1 of 11 intervals"),
        "{cost}"
    );
    assert!(!cost.contains("$0.000000"), "{cost}");
    assert!(line_with(&text, "promotion to live").contains(": no"));
}

/// L3: `quality.min_sessions` is a quality term, so it is met on the clusters
/// the quality test reads: five agreeing sessions, not ten eligible ones.
#[test]
fn min_sessions_is_met_on_the_quality_tests_own_sessions() {
    let calibrated = run(&config(6), &refuter_fixture());
    let text = calibrated.report.render();
    let line = line_with(&text, "quality.min_sessions 6");
    assert!(line.ends_with(": not met"), "{line}");
    assert!(
        line_with(&text, "with an eligible interval: 10").contains("sessions (sequence key)"),
        "{text}"
    );
}

fn outcome(cluster: usize, positive: bool, weight: f64) -> Outcome {
    Outcome {
        cluster,
        turns: 1,
        positive,
        weight,
        supported: weight > 0.0,
        matched: weight > 0.0,
        cost: Money::Priced(0.01),
        first_output_ms: Vec::new(),
    }
}

fn plan(resamples: u32) -> BootstrapPlan {
    BootstrapPlan { seed: 5, resamples }
}

/// L1: three weighted clusters of fifty leave about 4.5% of replicates with
/// no weight, more than the 2.5% tail, so the lower bound is filler. That is
/// `not evaluable`, never `fail`.
#[test]
fn sparse_support_reads_not_evaluable_never_fail() {
    let outcomes: Vec<Outcome> = (0..50)
        .map(|cluster| outcome(cluster, true, if cluster < 3 { 1.0 } else { 0.0 }))
        .collect();
    let estimate = estimate(&outcomes, plan(200));
    let low_at = ((1.0 - BOOTSTRAP_LEVEL) / 2.0 * 200.0).floor() as u32;
    assert!(
        estimate.bootstrap.undefined > low_at,
        "the fixture must be sparse: {} undefined",
        estimate.bootstrap.undefined
    );
    let result = quality_test(&estimate, Some(0.9));
    assert!(
        matches!(result, TestResult::NotEvaluable(_)),
        "sparse support read as {result:?}"
    );
}

/// S1: a replicate with no weight counts as 0.0 for the lower bound and 1.0
/// for the upper, never as the point estimate. With all weight zero both
/// bounds are filler; with a few undefined replicates inside the tail they
/// shift the bounds outward, by exactly the filler.
#[test]
fn an_undefined_replicate_widens_the_bound_asymmetrically() {
    let empty: Vec<Outcome> = (0..4).map(|cluster| outcome(cluster, true, 0.0)).collect();
    let bounds = bootstrap(&empty, plan(40));
    assert_eq!((bounds.lower, bounds.upper), (Some(0.0), Some(1.0)));
    assert_eq!(bounds.undefined, 40);

    // Forty clusters, five weighted with distinct rates and weights, so
    // about e^-5 of replicates draw none. So few weighted clusters make the
    // replicates coarse, and on many seeds both fillers land on the same
    // bound; the seed is therefore searched for one where substituting the
    // point estimate would move the lower and the upper bound, with every
    // undefined replicate inside the tail. The search reads only the
    // replicates, never `bootstrap`, so the assertion below is independent.
    let mut outcomes: Vec<Outcome> = Vec::new();
    for (cluster, (positive, negative)) in
        [(0.2, 0.8), (0.6, 0.9), (1.2, 0.8), (2.0, 0.5), (0.7, 0.7)]
            .into_iter()
            .enumerate()
    {
        outcomes.push(outcome(cluster, true, positive));
        outcomes.push(outcome(cluster, false, negative));
    }
    outcomes.extend((5..40).map(|cluster| outcome(cluster, true, 0.0)));
    let point = estimate(&outcomes, plan(1)).snips.unwrap();
    let resamples = 400;
    let tail = (1.0 - BOOTSTRAP_LEVEL) / 2.0;
    let low_at = (tail * resamples as f64).floor() as usize;
    let high_at = ((1.0 - tail) * resamples as f64).ceil() as usize - 1;
    let bounds_with = |replicates: &[Option<f64>], low: f64, high: f64| {
        let mut lows: Vec<f64> = replicates.iter().map(|v| v.unwrap_or(low)).collect();
        let mut highs: Vec<f64> = replicates.iter().map(|v| v.unwrap_or(high)).collect();
        lows.sort_by(f64::total_cmp);
        highs.sort_by(f64::total_cmp);
        (Some(lows[low_at]), Some(highs[high_at]))
    };
    let (seed, replicates) = (0..2_000u64)
        .map(|seed| {
            let plan = BootstrapPlan { seed, resamples };
            (seed, bootstrap_replicates(&outcomes, plan))
        })
        .find(|(_, replicates)| {
            let undefined = replicates.iter().filter(|value| value.is_none()).count();
            let filled = bounds_with(replicates, 0.0, 1.0);
            let substituted = bounds_with(replicates, point, point);
            (1..=low_at).contains(&undefined)
                && filled.0 != substituted.0
                && filled.1 != substituted.1
        })
        .expect("a seed that discriminates both bounds");
    let undefined = replicates.iter().filter(|value| value.is_none()).count();
    let bounds = bootstrap(&outcomes, BootstrapPlan { seed, resamples });
    assert_eq!(bounds.undefined as usize, undefined);
    assert!(!bounds.sparse, "{undefined} undefined, inside the tail");
    assert_eq!(
        (bounds.lower, bounds.upper),
        bounds_with(&replicates, 0.0, 1.0)
    );
}

/// S2: the ruled allowance is inclusive. A lower bound exactly
/// `QUALITY_ALLOWANCE` below the rate passes.
#[test]
fn a_lower_bound_exactly_the_allowance_below_the_rate_passes() {
    let rate = 0.9;
    assert_eq!(
        quality_gate(rate - QUALITY_ALLOWANCE, rate),
        TestResult::Pass
    );
    assert_eq!(
        quality_gate((rate - QUALITY_ALLOWANCE).next_down(), rate),
        TestResult::Fail
    );
}

/// S3: the ruled reduction is inclusive. A candidate priced exactly
/// `rules * (1 - COST_REDUCTION)` passes.
#[test]
fn a_cost_exactly_the_required_reduction_below_rules_passes() {
    let rules = 0.055;
    assert_eq!(
        cost_gate(rules * (1.0 - COST_REDUCTION), rules),
        TestResult::Pass
    );
    assert_eq!(
        cost_gate((rules * (1.0 - COST_REDUCTION)).next_up(), rules),
        TestResult::Fail
    );
}

/// L2: estimated usage leaves cached input at zero, so pricing it as measured
/// overprices any turn that read from cache. It is unpriced.
#[test]
fn estimated_usage_is_unpriced_never_priced_as_measured() {
    let mut script = Script::named("acme/ada/one#g0");
    let mut turn = script.begin();
    script.route(&mut turn, Spec::new().rate_card(CARD).decision());
    let at = script.clock + 10;
    script.delta_at(&turn, at, "answer");
    // The decision predicted 600 of 1000 tokens from cache; the estimate
    // cannot say how many were.
    script.complete(&turn, usage(1_000, Accounting::Estimated));
    script.review(&[&turn], on_track());
    let calibrated = run(&config(1), &[script]);
    assert_eq!(
        calibrated.evidence.intervals[0].turns[0].cost,
        Money::Unpriced
    );
}

/// One agreeing `shadow` session: the learned choice is `rules` on `opus`.
fn agreeing_with(id: &str, positive: bool) -> Script {
    let spec = Spec::new()
        .corrected(Strategy::Rules, 0.01, 800.0)
        .corrected(Strategy::Efficient, 0.002, 600.0)
        .corrected(Strategy::Capable, 0.01, 800.0);
    session(id, spec, 1_000, positive)
}

/// The earlier fixer's fixture: twenty agreeing `shadow` sessions, seventeen
/// positive. Both policies served the same route on every interval, so no
/// quality loss can exist.
fn noisy_agreeing_fixture() -> Vec<Script> {
    (0..20)
        .map(|at| agreeing_with(&format!("acme/ada/agree{at:02}#g0"), at >= 3))
        .collect()
}

/// The M10 review-fix ruling of 2026-09-30: test 1 is the paired statistic.
/// On twenty agreeing sessions, seventeen positive, the learned lower bound
/// sits well below the `rules` point estimate (the unpaired form reads
/// `fail` on sample noise alone), while the paired difference is exactly
/// zero on every resample.
#[test]
fn the_agreeing_quality_test_is_paired_and_passes_on_sample_noise() {
    let calibrated = run(&config(20), &noisy_agreeing_fixture());
    let promotion = &calibrated.report.promotion;
    assert_eq!((promotion.agreeing, promotion.intervals), (20, 20));
    assert_eq!(promotion.agreeing_sessions, 20);
    // The control: the unpaired comparison the ruling retired does fail
    // here, so a pass below is the paired statistic and not a lenient
    // fixture.
    let learned = calibrated.report.estimate(Candidate::Learned).unwrap();
    let rules = calibrated
        .report
        .estimate(Candidate::Fixed(Strategy::Rules))
        .unwrap();
    assert_eq!(rules.snips, Some(0.85));
    assert_eq!(
        quality_gate(learned.bootstrap.lower.unwrap(), rules.snips.unwrap()),
        TestResult::Fail,
        "the unpaired form must fail on this fixture: lower {:?}",
        learned.bootstrap.lower
    );
    assert_eq!(promotion.quality_agreeing.result, TestResult::Pass);
    let text = calibrated.report.render();
    println!("{text}");
    let line = line_with(
        &text,
        "quality, on the intervals where learned agreed with rules",
    );
    assert!(line.ends_with(": pass"), "{line}");
    assert!(line.contains("learned minus rules"), "{line}");
}

/// The agreeing set below `quality.min_sessions` is not evaluable: twenty
/// sessions against a minimum of twenty-one.
#[test]
fn the_agreeing_quality_test_below_min_sessions_is_not_evaluable() {
    let calibrated = run(&config(21), &noisy_agreeing_fixture());
    let promotion = &calibrated.report.promotion;
    assert!(
        matches!(
            promotion.quality_agreeing.result,
            TestResult::NotEvaluable(_)
        ),
        "{:?}",
        promotion.quality_agreeing
    );
    assert!(!promotion.promotable());
    let text = calibrated.report.render();
    assert!(
        line_with(&text, "quality.min_sessions 21").ends_with(": not met"),
        "{text}"
    );
}

/// A `live` turn with exploration possible over `efficient`, at propensity
/// 0.5. Nothing passes, so the learned choice is `rules` on `opus`; an
/// explored turn served `efficient` on `haiku` instead.
fn live_turn(explored: bool) -> Spec {
    let spec = Spec::new().live().propensity(0.5);
    let exploration = |rate: f64| ExplorationEvidence {
        draw: Draw { rate, member: 0 },
        possible: true,
        set: vec![Strategy::Efficient],
    };
    if explored {
        spec.chosen(haiku())
            .choice(LearnedChoice::Explore {
                strategy: Strategy::Efficient,
                member: 0,
            })
            .exploration(exploration(0.0))
    } else {
        spec.chosen(opus()).exploration(exploration(0.9))
    }
}

/// One `live` session of one-turn intervals, each `(explored, positive)`.
fn live_session(id: &str, intervals: &[(bool, bool)]) -> Script {
    let mut script = Script::named(id);
    for (explored, positive) in intervals {
        let turn = script.turn(live_turn(*explored).decision());
        script.review(&[&turn], if *positive { on_track() } else { off_track() });
    }
    script
}

/// The 2026-09-30 ruling expected the paired difference to be nonzero in
/// `live` with exploration. It is not, and no fixture can make it so: an
/// agreeing interval is one where the learned action is the
/// `rules` action on every turn, and a candidate's weight reads only its
/// action, the served target and the recorded propensity. So both sides
/// carry the same weight on every agreeing interval, explored or not, and
/// the paired difference is zero in `live` too. Explored turns zero both
/// sides together; they never separate them.
#[test]
fn on_agreeing_live_intervals_learned_and_rules_carry_the_same_weight() {
    let scripts: Vec<Script> = (0..20)
        .map(|at| {
            live_session(
                &format!("acme/ada/live{at:02}#g0"),
                // An explored negative, a served positive, and on every
                // fourth session a served negative.
                &[(true, false), (false, true), (false, at % 4 != 0)],
            )
        })
        .collect();
    let calibrated = run(&config(20), &scripts);
    let intervals = &calibrated.evidence.intervals;
    assert_eq!(intervals.len(), 60);
    assert_eq!(calibrated.report.promotion.agreeing, 60, "all agree");
    let mut explored_zero = 0;
    for interval in intervals {
        let learned = interval.outcome(Candidate::Learned);
        let rules = interval.outcome(Candidate::Fixed(Strategy::Rules));
        assert_eq!(learned, rules, "{interval:?}");
        explored_zero += usize::from(learned.weight == 0.0);
    }
    assert_eq!(explored_zero, 20, "every explored interval zeroes both");
    assert_eq!(
        calibrated.report.promotion.quality_agreeing.result,
        TestResult::Pass
    );
}

/// A paired outcome pair on one interval: the learned and the `rules` weight.
fn pair(cluster: usize, positive: bool, learned: f64, rules: f64) -> (Outcome, Outcome) {
    (
        outcome(cluster, positive, learned),
        outcome(cluster, positive, rules),
    )
}

fn split(pairs: Vec<(Outcome, Outcome)>) -> (Vec<Outcome>, Vec<Outcome>) {
    pairs.into_iter().unzip()
}

/// The statistic itself detects loss where the weights differ: thirty
/// clusters where the learned side weights a negative that `rules` does not,
/// so learned is 0.5 and `rules` 2/3 in every cluster. The paired bound is
/// the difference, -1/6, and the gate fails. The agreeing set never looks
/// like this (see the `live` test above); the pure function is where the
/// failing direction is shown.
#[test]
fn the_paired_bound_fails_a_loss_the_weights_show() {
    let (learned, rules) = split(
        (0..30)
            .flat_map(|cluster| {
                [
                    pair(cluster, true, 1.0, 1.0),
                    pair(cluster, true, 1.0, 1.0),
                    pair(cluster, false, 1.0, 1.0),
                    pair(cluster, false, 1.0, 0.0),
                ]
            })
            .collect(),
    );
    let bounds = paired_bootstrap(&learned, &rules, plan(200));
    assert!(!bounds.sparse);
    let lower = bounds.lower.unwrap();
    assert!((lower + 1.0 / 6.0).abs() < 1e-12, "{lower}");
    assert_eq!(paired_quality_gate(lower), TestResult::Fail);
    // With no difference, the same clusters pass.
    let bounds = paired_bootstrap(&learned, &learned, plan(200));
    assert_eq!(bounds.lower, Some(0.0));
    assert_eq!(paired_quality_gate(0.0), TestResult::Pass);
}

/// The paired allowance is inclusive, as ruling 13's is.
#[test]
fn a_paired_lower_bound_exactly_the_allowance_below_zero_passes() {
    assert_eq!(paired_quality_gate(-QUALITY_ALLOWANCE), TestResult::Pass);
    assert_eq!(
        paired_quality_gate((-QUALITY_ALLOWANCE).next_down()),
        TestResult::Fail
    );
}

/// The paired bootstrap resamples clusters, not intervals. Copying every
/// interval three times inside its own cluster scales each cluster's sums
/// exactly, and keeps the number of clusters each replicate draws, so every
/// replicate is bitwise the same. A resampler over intervals draws three
/// times as many units from the stream and gives different replicates.
#[test]
fn the_paired_bootstrap_resamples_clusters_not_intervals() {
    // Twelve clusters with different paired differences, so the replicates
    // spread and a changed draw shows.
    let one: Vec<(Outcome, Outcome)> = (0..12)
        .flat_map(|cluster| {
            [
                pair(cluster, true, 1.0, 1.0),
                pair(cluster, false, 1.0, (cluster % 3) as f64),
                pair(cluster, cluster % 2 == 0, (cluster % 4) as f64, 1.0),
            ]
        })
        .collect();
    let tripled: Vec<(Outcome, Outcome)> = one
        .iter()
        .flat_map(|pair| [pair.clone(), pair.clone(), pair.clone()])
        .collect();
    let (learned, rules) = split(one);
    let (learned3, rules3) = split(tripled);
    let plan = plan(200);
    let replicates = paired_replicates(&learned, &rules, plan);
    let distinct = {
        let mut values: Vec<f64> = replicates.iter().flatten().copied().collect();
        values.sort_by(f64::total_cmp);
        values.dedup();
        values.len()
    };
    assert!(
        distinct > 20,
        "the fixture must spread: {distinct} distinct"
    );
    assert_eq!(paired_replicates(&learned3, &rules3, plan), replicates);
}

/// The support and sparse guards hold for the paired test: a `live` set
/// where every agreeing interval explored away has no weight, and one where
/// three of fifty sessions hold weight is sparse. Both read `not evaluable`,
/// never `pass` and never `fail`.
///
/// `quality.min_sessions` counts only the sessions where learned has weight,
/// so from the report the no-weight guard answers only under a minimum of
/// zero, and the sparse one only under a minimum the three weighted sessions
/// meet. Under any larger minimum the sessions guard answers first
/// (`learner_offline_gates.rs` holds the order).
#[test]
fn the_paired_test_without_support_or_on_sparse_support_is_not_evaluable() {
    let unweighted: Vec<Script> = (0..20)
        .map(|at| live_session(&format!("acme/ada/live{at:02}#g0"), &[(true, true)]))
        .collect();
    let calibrated = run(&config(0), &unweighted);
    let test = calibrated.report.promotion.quality_agreeing;
    assert_eq!(test.intervals, 20);
    assert!(
        matches!(test.result, TestResult::NotEvaluable(reason) if reason.contains("learned has no interval")),
        "{test:?}"
    );

    let sparse: Vec<Script> = (0..50)
        .map(|at| live_session(&format!("acme/ada/live{at:02}#g0"), &[(at >= 3, true)]))
        .collect();
    let calibrated = run(&config(3), &sparse);
    let test = calibrated.report.promotion.quality_agreeing;
    assert!(
        matches!(test.result, TestResult::NotEvaluable(reason) if reason.contains("filler")),
        "{test:?}"
    );
}
