// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The offline calibrator over crafted logs (milestone M10 of
//! `agent-docs/PLAN-online-routing-learner.md`): weights, exclusions, the
//! estimand's label, cost, latency, the promotion summary and the artifact.
//!
//! The store-facing half (enumeration, read-only, drift, rollback) is in
//! `learner_offline_stores.rs`.

mod learning_support;

use learning_support::*;
use roundhouse_core::classify::TierChoice;
use roundhouse_core::classify::TurnComplexity;
use roundhouse_core::control::ProjectId;
use roundhouse_core::routing::learn::offline::estimate::{
    bootstrap_replicates, estimate, trajectory_probability, trajectory_weight, weighted_p50,
};
use roundhouse_core::routing::learn::offline::{
    ArtifactPrior, BootstrapPlan, CORRECTED_QUOTE_LABEL, Calibrated, CalibrationConfig, Candidate,
    Cause, CostEstimate, DriftCheck, ESTIMAND_LABEL, Evidence, Money, Outcome, QualityMinimum,
    SessionLog, Source, SplitMix64, TestResult, TurnTrace, assemble, sidecar_bytes,
};
use roundhouse_core::routing::learn::{
    Artifact, Draw, ExplorationEvidence, LearnedChoice, Strategy, StrategySet, epoch_of,
};
use roundhouse_core::routing::{ProviderPricing, Target};
use roundhouse_core::session::Exclusion;

fn config() -> CalibrationConfig {
    CalibrationConfig {
        project: ProjectId::new("acme"),
        strategies: StrategySet::new(vec![
            Strategy::Rules,
            Strategy::Efficient,
            Strategy::Capable,
        ])
        .unwrap(),
        prior: ArtifactPrior::Credit,
        quality: QualityMinimum { min_sessions: 2 },
        latency_limit_ms: 10_000,
        bootstrap: BootstrapPlan {
            seed: 7,
            resamples: 200,
        },
        cutoff: None,
    }
}

fn run(scripts: &[&Script]) -> Calibrated {
    run_with(&config(), scripts)
}

fn run_with(config: &CalibrationConfig, scripts: &[&Script]) -> Calibrated {
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

fn verdict(positive: bool) -> roundhouse_core::validate::Verdict {
    if positive { on_track() } else { off_track() }
}

const CARD: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

/// A shadow session of one-turn intervals, one per label, priced at [`CARD`],
/// with every plan's quote corrected ($0.01 and 800 ms on `opus`, $0.001 and
/// 600 ms on `haiku`).
fn shadow(id: &str, labels: &[bool]) -> Script {
    let mut script = Script::named(id);
    for positive in labels {
        let turn = script.turn(corrected(Spec::new()).rate_card(CARD).decision());
        script.review(&[&turn], verdict(*positive));
    }
    script
}

/// A live turn of the reviewer's uniform A/B logger: A is `haiku`, which only
/// `efficient` plans, and B is `opus`, the `rules` target. Each side is served
/// with probability one half.
fn ab_spec(served_a: bool) -> Spec {
    let spec = Spec::new()
        .live()
        .plans(vec![
            (Strategy::Rules, opus()),
            (Strategy::Efficient, haiku()),
            (Strategy::Capable, opus()),
        ])
        .propensity(0.5);
    let exploration = |rate: f64| ExplorationEvidence {
        draw: Draw { rate, member: 0 },
        possible: true,
        set: vec![Strategy::Efficient],
    };
    if served_a {
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

/// Every plan of the fixture's default targets with its quote corrected.
fn corrected(spec: Spec) -> Spec {
    spec.corrected(Strategy::Rules, 0.01, 800.0)
        .corrected(Strategy::Efficient, 0.001, 600.0)
        .corrected(Strategy::Capable, 0.01, 800.0)
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line with `{needle}` in:\n{text}"))
}

/// The lines of one candidate's block.
fn block(text: &str, candidate: Candidate) -> Vec<&str> {
    let header = format!("### {}", candidate.label());
    text.lines()
        .skip_while(|line| *line != header)
        .skip(1)
        .take_while(|line| !line.starts_with('#'))
        .collect()
}

#[test]
fn the_same_manifest_gives_byte_identical_artifacts() {
    let one = shadow("acme/ada/one#g0", &[true, false]);
    let two = shadow("acme/ada/two#g0", &[true]);
    let first = run(&[&one, &two]);
    let second = run(&[&two, &one]);
    assert!(!first.evidence.prior.is_empty(), "a prior was written");
    assert_eq!(first.artifact, second.artifact);
    assert_eq!(first.report.render(), second.report.render());
    assert_eq!(first.input, second.input);
}

/// The digest M9 records in `CompiledUnder` is the artifact's SHA-256, so two
/// calibrations of one manifest must give one digest.
#[test]
fn the_artifact_sha256_is_reproducible_from_the_same_manifest() {
    let one = shadow("acme/ada/one#g0", &[true]);
    let first = Artifact::parse(&run(&[&one]).artifact).unwrap();
    let second = Artifact::parse(&run(&[&one]).artifact).unwrap();
    assert_eq!(first.sha256(), second.sha256());
    assert_eq!(first.epoch(), second.epoch());
}

#[test]
fn the_artifact_round_trips_through_the_server_parser() {
    let one = shadow("acme/ada/one#g0", &[true, true]);
    let calibrated = run(&[&one]);
    let parsed = Artifact::parse(&calibrated.artifact).expect("the server's parser accepts it");
    assert_eq!(
        parsed.epoch(),
        epoch_of(&calibrated.artifact, parsed.strategies())
    );
    assert_eq!(parsed.strategies(), &config().strategies);
    assert_eq!(parsed.manifest_digest(), calibrated.input.digest());
    assert_eq!(parsed.source_commit(), "test-commit");
    for ((key, strategy), units) in &calibrated.evidence.prior {
        assert_eq!(parsed.prior().get(key, *strategy), *units);
    }
    // Two positive intervals credit `rules` and `capable` (both on `opus`)
    // at each level; `efficient` planned `haiku` and earns nothing.
    let l2 = input(
        roundhouse_core::routing::Tier::Capable,
        roundhouse_core::routing::learn::Band::None,
    )
    .keys()[0];
    assert_eq!(parsed.prior().get(&l2, Strategy::Rules).pos, 2_000);
    assert_eq!(parsed.prior().get(&l2, Strategy::Efficient).n, 0);
}

#[test]
fn a_zero_prior_manifest_writes_no_units() {
    let one = shadow("acme/ada/one#g0", &[true]);
    let config = CalibrationConfig {
        prior: ArtifactPrior::Zero,
        ..config()
    };
    let parsed = Artifact::parse(&run_with(&config, &[&one]).artifact).unwrap();
    let l2 = input(
        roundhouse_core::routing::Tier::Capable,
        roundhouse_core::routing::learn::Band::None,
    )
    .keys()[0];
    assert_eq!(parsed.prior().get(&l2, Strategy::Rules).n, 0);
}

/// Jev's tier answers reach the store as counts for a prior computed on the
/// turn. They are never review evidence, so the artifact never carries them.
#[test]
fn jev_answers_never_enter_the_artifact_prior() {
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(Spec::new().decision());
    script.intent(&turn);
    script.answer(
        &turn,
        classification(TurnComplexity::Routine, Some(TierChoice::Efficient)),
    );
    let calibrated = run(&[&script]);
    assert!(
        calibrated.evidence.prior.is_empty(),
        "a Jev answer without a review credits nothing: {:?}",
        calibrated.evidence.prior
    );
    assert_eq!(
        calibrated.report.agreement.answered, 1,
        "but it is compared"
    );
}

#[test]
fn the_sidecar_time_does_not_change_the_epoch_id() {
    let one = shadow("acme/ada/one#g0", &[true]);
    let calibrated = run(&[&one]);
    let early = sidecar_bytes(&calibrated.parsed, 1_000, "host-a");
    let late = sidecar_bytes(&calibrated.parsed, 9_000_000, "host-b");
    assert_ne!(early, late, "the sidecar holds the time and the host");
    let again = run(&[&one]);
    assert_eq!(calibrated.artifact, again.artifact);
    assert_eq!(calibrated.parsed.epoch(), again.parsed.epoch());
    let sidecar: serde_json::Value = serde_json::from_slice(&late).unwrap();
    assert_eq!(sidecar["epoch"], calibrated.parsed.epoch().to_string());
    assert_eq!(sidecar["artifact_sha256"], calibrated.parsed.sha256());
    let artifact = String::from_utf8(calibrated.artifact).unwrap();
    assert!(!artifact.contains("created_at") && !artifact.contains("host"));
}

/// An explored turn whose recorded draw no longer gives its recorded member
/// does not replay: its interval is excluded, and the count says why.
#[test]
fn replay_equivalence_fails_on_a_changed_draw() {
    let explored = |draw_member: u64| {
        Spec::new()
            .live()
            .plans(vec![
                (Strategy::Rules, opus()),
                (Strategy::Efficient, haiku()),
                (Strategy::Capable, opus()),
            ])
            .chosen(haiku())
            .propensity(0.025)
            .choice(LearnedChoice::Explore {
                strategy: Strategy::Efficient,
                member: 0,
            })
            .exploration(ExplorationEvidence {
                draw: Draw {
                    rate: 0.01,
                    member: draw_member,
                },
                possible: true,
                set: vec![Strategy::Efficient, Strategy::Capable],
            })
            .decision()
    };
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(explored(4));
    script.review(&[&turn], on_track());
    let changed = script.turn(explored(5));
    script.review(&[&changed], on_track());
    let calibrated = run(&[&script]);
    assert_eq!(
        calibrated.evidence.intervals.len(),
        1,
        "the recorded draw replays"
    );
    assert_eq!(
        calibrated.evidence.exclusions.get(&Cause::ReplayMismatch),
        Some(&1),
        "draw 5 gives member 1, not the recorded member 0"
    );
}

/// The reviewer's case: a uniform A/B logger, a candidate that always picks
/// A, and rewards A = 0 and B = 1. Weights over matching turns only give 0.5
/// and 1/3; the product over every turn gives the true 0.
#[test]
fn one_mismatched_turn_zeroes_the_trajectory_weight() {
    let a = TurnTrace {
        propensity: 0.5,
        matched: true,
        supported: true,
    };
    let b = TurnTrace {
        matched: false,
        ..a
    };
    assert_eq!(trajectory_weight(&[a]), 2.0);
    assert_eq!(trajectory_weight(&[b]), 0.0);
    assert_eq!(
        trajectory_weight(&[a, b]),
        0.0,
        "one miss zeroes a longer trajectory"
    );

    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(ab_spec(true).decision());
    script.review(&[&turn], off_track());
    let turn = script.turn(ab_spec(false).decision());
    script.review(&[&turn], on_track());
    let calibrated = run(&[&script]);
    let always_a = calibrated
        .report
        .estimate(Candidate::Fixed(Strategy::Efficient))
        .unwrap();
    assert_eq!(always_a.ips, 0.0, "not 0.5");
    assert_eq!(always_a.snips, Some(0.0), "not 1/3");
}

#[test]
fn an_explored_turn_uses_the_recorded_propensity() {
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(ab_spec(true).propensity(0.05).decision());
    script.review(&[&turn], on_track());
    let calibrated = run(&[&script]);
    let interval = &calibrated.evidence.intervals[0];
    assert_eq!(interval.turns[0].propensity, 0.05);
    assert_eq!(
        interval
            .outcome(Candidate::Fixed(Strategy::Efficient))
            .weight,
        20.0
    );
}

#[test]
fn trajectory_probability_multiplies_turn_probabilities() {
    let turn = |propensity| TurnTrace {
        propensity,
        matched: true,
        supported: true,
    };
    assert_eq!(trajectory_probability(&[turn(0.5), turn(0.25)]), 0.125);
    assert_eq!(trajectory_weight(&[turn(0.5), turn(0.25)]), 8.0);

    let mut script = Script::named("acme/ada/one#g0");
    let first = script.turn(ab_spec(true).decision());
    let second = script.turn(ab_spec(true).propensity(0.25).decision());
    script.review(&[&first, &second], on_track());
    let calibrated = run(&[&script]);
    let outcome = calibrated.evidence.intervals[0].outcome(Candidate::Fixed(Strategy::Efficient));
    assert_eq!(outcome.weight, 8.0);
}

/// The reviewer's fixture: four logged sessions, one per A/B history, two
/// one-turn intervals each. The second interval is positive only if both
/// turns took A. Always-A over a whole conversation scores 1.0; interval-local
/// weights target the conditional interval value, 0.75, and the report says
/// that and nothing else.
#[test]
fn the_conditional_interval_value_is_labeled_as_such_and_never_as_session_value() {
    let mut scripts = Vec::new();
    for (index, (a1, a2)) in [(true, true), (true, false), (false, true), (false, false)]
        .into_iter()
        .enumerate()
    {
        let mut script = Script::named(&format!("acme/ada/h{index}#g0"));
        let turn = script.turn(ab_spec(a1).decision());
        script.review(&[&turn], verdict(a1));
        let turn = script.turn(ab_spec(a2).decision());
        script.review(&[&turn], verdict(a1 && a2));
        scripts.push(script);
    }
    let calibrated = run(&scripts.iter().collect::<Vec<_>>());
    let always_a = calibrated
        .report
        .estimate(Candidate::Fixed(Strategy::Efficient))
        .unwrap();
    assert_eq!(always_a.snips, Some(0.75));
    let text = calibrated.report.render();
    let lines = block(&text, Candidate::Fixed(Strategy::Efficient));
    let snips = lines
        .iter()
        .find(|line| line.contains("self-normalized"))
        .unwrap();
    assert!(
        snips.contains(ESTIMAND_LABEL) && snips.ends_with("0.7500"),
        "{snips}"
    );
    assert!(!snips.contains("1.0000"));
    for forbidden in ["session value", "deployment value", "policy value"] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "the report names `{forbidden}`:\n{text}"
        );
    }
    for line in text
        .lines()
        .filter(|line| line.contains("$") || line.contains("positive rate,"))
    {
        assert!(
            line.contains(ESTIMAND_LABEL)
                || line.contains(CORRECTED_QUOTE_LABEL)
                || line.contains("factual")
                || line.contains("classifier"),
            "an unlabeled number: {line}"
        );
    }
}

#[test]
fn the_support_census_counts_trajectories_with_zero_logging_probability() {
    // Shadow: `rules` serves `opus` with probability 1, so `haiku` has zero.
    let one = shadow("acme/ada/one#g0", &[true, false]);
    // Live and exploring: `haiku` is a member, so it has positive probability
    // even on the turn that served `opus`.
    let mut two = Script::named("acme/ada/two#g0");
    let turn = two.turn(ab_spec(false).decision());
    two.review(&[&turn], on_track());
    let calibrated = run(&[&one, &two]);
    let efficient = calibrated
        .report
        .estimate(Candidate::Fixed(Strategy::Efficient))
        .unwrap();
    assert_eq!((efficient.supported, efficient.intervals), (1, 3));
    assert_eq!(efficient.weighted, 0, "supported is not matched");
    let rules = calibrated
        .report
        .estimate(Candidate::Fixed(Strategy::Rules))
        .unwrap();
    assert_eq!(rules.supported, 3);
    let text = calibrated.report.render();
    let census = block(&text, Candidate::Fixed(Strategy::Efficient))
        .into_iter()
        .find(|line| line.starts_with("support census"))
        .unwrap();
    assert!(census.contains("1 of 3") && census.contains("2 have zero logging probability"));
}

#[test]
fn intervals_are_excluded_by_cause_and_the_counts_are_reported() {
    let mut script = Script::named("acme/ada/one#g0");
    // Eligible.
    let turn = script.turn(Spec::new().decision());
    script.review(&[&turn], on_track());
    // Unknown: the judge said it lacked context.
    let turn = script.turn(Spec::new().decision());
    script.review(&[&turn], blind());
    // Failover: a second dispatch after a failed one.
    let mut turn = script.begin();
    script.route(&mut turn, Spec::new().chosen(haiku()).decision());
    script.route(&mut turn, Spec::new().failed_before().decision());
    script.complete(&turn, unmeasured());
    script.review(&[&turn], on_track());
    // No learned row.
    let turn = script.turn(unlearned(opus()));
    script.review(&[&turn], on_track());
    // Mixed epoch.
    let first = script.turn(Spec::new().decision());
    let second = script.turn(Spec::new().epoch(other_epoch()).decision());
    script.review(&[&first, &second], on_track());
    // Another credit revision.
    let turn = script.turn(Spec::new().credit_revision(99).decision());
    script.review(&[&turn], on_track());
    // A propensity below 1 on a turn that could not explore.
    let turn = script.turn(Spec::new().propensity(0.5).decision());
    script.review(&[&turn], on_track());
    // Three matched turns whose propensities multiply to below the smallest
    // float: the weight of a candidate that matched them is not finite.
    let exploration = ExplorationEvidence {
        draw: Draw {
            rate: 0.9,
            member: 0,
        },
        possible: true,
        set: vec![Strategy::Efficient],
    };
    let tiny: Vec<Turn> = (0..3)
        .map(|_| {
            script.turn(
                Spec::new()
                    .live()
                    .propensity(1e-110)
                    .exploration(exploration.clone())
                    .decision(),
            )
        })
        .collect();
    script.review(&tiny.iter().collect::<Vec<_>>(), on_track());

    let calibrated = run(&[&script]);
    let exclusions = &calibrated.evidence.exclusions;
    for (cause, count) in [
        (Cause::Screen(Exclusion::UnknownLabel), 1),
        (Cause::Screen(Exclusion::FailoverInInterval), 1),
        (Cause::Screen(Exclusion::MissingRow), 1),
        (Cause::Screen(Exclusion::MixedEpoch), 1),
        (Cause::Screen(Exclusion::OtherCreditRevision), 1),
        (Cause::ReplayMismatch, 1),
        (Cause::NonFiniteWeight, 1),
    ] {
        assert_eq!(exclusions.get(&cause), Some(&count), "{cause:?}");
    }
    assert_eq!(calibrated.evidence.intervals.len(), 1);
    assert_eq!(calibrated.report.accepted_reviews, 8);
    let text = calibrated.report.render();
    for cause in Cause::ALL {
        assert_eq!(
            line_with(&text, &format!("- {}:", cause.label())),
            format!("- {}: 1", cause.label())
        );
    }
}

/// Credit and the calibrator screen with one function, so the session fold's
/// own cause counts equal the calibrator's screen exclusions.
#[test]
fn the_screen_exclusions_equal_the_session_folds_causes() {
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(Spec::new().decision());
    script.review(&[&turn], blind());
    let turn = script.turn(unlearned(opus()));
    script.review(&[&turn], on_track());
    let first = script.turn(Spec::new().decision());
    let second = script.turn(Spec::new().epoch(other_epoch()).decision());
    script.review(&[&first, &second], off_track());
    let evidence = run(&[&script]).evidence;
    let causes = evidence.fold_causes;
    let count = |exclusion| {
        evidence
            .exclusions
            .get(&Cause::Screen(exclusion))
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(causes.unknown_label, count(Exclusion::UnknownLabel));
    assert_eq!(causes.missing_row, count(Exclusion::MissingRow));
    assert_eq!(causes.mixed_epoch, count(Exclusion::MixedEpoch));
    assert_eq!(
        causes.failover_in_interval,
        count(Exclusion::FailoverInInterval)
    );
    assert_eq!(
        causes.other_credit_revision,
        count(Exclusion::OtherCreditRevision)
    );
    assert_eq!(
        (causes.unknown_label, causes.missing_row, causes.mixed_epoch),
        (1, 1, 1)
    );
}

fn outcome(cluster: usize, positive: bool) -> Outcome {
    Outcome {
        cluster,
        turns: 1,
        positive,
        weight: 1.0,
        supported: true,
        matched: true,
        cost: Money::Priced(0.0),
        first_output_ms: Vec::new(),
    }
}

/// Session 0 holds three positive intervals and session 1 one negative. A
/// replicate draws two whole sessions, so it can only be 1.0, 0.75 or 0.0;
/// resampling intervals one at a time would also give 0.5 and 0.25.
#[test]
fn the_bootstrap_resamples_sessions_with_the_recorded_seed() {
    let outcomes = vec![
        outcome(0, true),
        outcome(0, true),
        outcome(0, true),
        outcome(1, false),
    ];
    let plan = BootstrapPlan {
        seed: 11,
        resamples: 400,
    };
    let replicates = bootstrap_replicates(&outcomes, plan);
    assert_eq!(replicates.len(), 400);
    for replicate in &replicates {
        let value = replicate.expect("every cluster has weight");
        assert!(
            [0.0, 0.75, 1.0].contains(&value),
            "a replicate of {value} split a session"
        );
    }
    assert!(replicates.contains(&Some(0.75)) && replicates.contains(&Some(0.0)));
    assert_eq!(
        replicates,
        bootstrap_replicates(&outcomes, plan),
        "the seed reproduces"
    );
    assert_ne!(
        replicates,
        bootstrap_replicates(&outcomes, BootstrapPlan { seed: 12, ..plan }),
        "another seed is another stream"
    );

    let one = shadow("acme/ada/one#g0", &[true, true]);
    let text = run(&[&one]).report.render();
    assert!(line_with(&text, "bootstrap:").contains("seed 7"));
}

/// The published first output of SplitMix64 for seed 0, so a change to the
/// stream fails here rather than silently moving every recorded interval.
#[test]
fn the_bootstrap_stream_matches_splitmix64() {
    let mut stream = SplitMix64::new(0);
    assert_eq!(stream.next_u64(), 0xE220_A839_7B1D_CDAF);
    assert_eq!(stream.next_u64(), 0x6E78_9E6A_A1B9_65F4);
}

fn usage(cached: u64) -> roundhouse_core::event::Usage {
    measured(cached)
}

#[test]
fn measured_cost_uses_the_recorded_rate_card() {
    let card = CARD;
    let mut script = Script::named("acme/ada/one#g0");
    let mut turn = script.begin();
    script.route(&mut turn, Spec::new().rate_card(card).decision());
    let at = script.clock + 10;
    script.delta_at(&turn, at, "answer");
    script.complete(&turn, usage(600));
    script.review(&[&turn], on_track());
    let calibrated = run(&[&script]);
    let expected = card.price(&usage(600));
    assert!(expected > 0.0 && expected != 0.01, "not the quote");
    assert_eq!(
        calibrated.evidence.intervals[0].turns[0].cost,
        Money::Priced(expected)
    );
    assert_eq!(calibrated.report.rules.cost, CostEstimate::Priced(expected));
    assert_eq!(
        calibrated.report.estimate(Candidate::Learned).unwrap().cost,
        CostEstimate::Priced(expected)
    );
}

#[test]
fn an_unpriced_local_plan_is_reported_as_unpriced_never_zero() {
    let local = || {
        Spec::new()
            .chosen(qwen())
            .plans(vec![
                (Strategy::Rules, qwen()),
                (Strategy::Efficient, qwen()),
                (Strategy::Capable, opus()),
            ])
            .decision()
    };
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(local());
    script.review(&[&turn], on_track());
    let calibrated = run(&[&script]);
    assert_eq!(
        calibrated.evidence.intervals[0].turns[0].cost,
        Money::Unpriced
    );
    let learned = calibrated.report.estimate(Candidate::Learned).unwrap();
    assert_eq!(learned.cost, CostEstimate::Unpriced);
    assert_eq!(calibrated.report.rules.cost, CostEstimate::Unpriced);
    assert!(matches!(
        calibrated.report.promotion.cost.result,
        TestResult::NotEvaluable(_)
    ));
    let text = calibrated.report.render();
    for line in text.lines().filter(|line| line.contains("cost")) {
        assert!(
            !line.contains("$0.000000"),
            "unpriced printed as free: {line}"
        );
    }
    assert!(line_with(&text, "cost per interval, factual").contains("unpriced"));
}

/// First output is measured from `TurnStarted`, so the overhead before
/// dispatch counts: 300 ms of it here, which a measurement from `Routed`
/// would drop.
#[test]
fn the_p50_first_output_is_measured_from_turn_start_in_the_log() {
    let mut script = Script::named("acme/ada/one#g0");
    let mut turns = Vec::new();
    for (start, to_output) in [(10_000, 500), (20_000, 900), (30_000, 700)] {
        let mut turn = script.begin_at(start);
        script.route_at(&mut turn, start + 300, Spec::new().decision());
        script.delta_at(&turn, start + to_output, "answer");
        script.complete_at(&turn, start + to_output + 50, unmeasured());
        turns.push(turn);
    }
    // A turn that never completed supplies no sample.
    let mut turn = script.begin_at(40_000);
    script.route_at(&mut turn, 40_100, Spec::new().decision());
    script.delta_at(&turn, 40_200, "partial");
    script.incomplete(&turn);
    turns.push(turn);
    script.review(&turns.iter().collect::<Vec<_>>(), on_track());
    let calibrated = run(&[&script]);
    assert_eq!(calibrated.report.rules.p50_first_output_ms, Some(700));
    assert_eq!(
        calibrated
            .report
            .estimate(Candidate::Learned)
            .unwrap()
            .p50_first_output_ms,
        Some(700)
    );
    assert_eq!(
        weighted_p50([(500, 1.0), (900, 1.0), (700, 1.0)]),
        Some(700)
    );
    assert_eq!(weighted_p50([(500, 1.0), (900, 3.0)]), Some(900));
}

#[test]
fn the_promotion_summary_states_each_of_the_three_ruled_tests_and_its_result() {
    let one = shadow("acme/ada/one#g0", &[true, true]);
    let two = shadow("acme/ada/two#g0", &[true]);
    let calibrated = run(&[&one, &two]);
    let promotion = calibrated.report.promotion;
    // In shadow with nothing passing, the learned choice is `rules` on every
    // turn: every interval agrees, quality and latency hold, and the
    // corrected quotes show no cost saving.
    assert_eq!((promotion.agreeing, promotion.intervals), (3, 3));
    assert_eq!(promotion.quality_agreeing.result, TestResult::Pass);
    assert_eq!(promotion.quality_full.result, TestResult::Pass);
    assert_eq!(promotion.cost.result, TestResult::Fail);
    assert_eq!(promotion.latency.result, TestResult::Pass);
    assert_eq!(promotion.latency.p50_ms, Some(800));
    assert_eq!(
        (promotion.agreeing_sessions, promotion.min_sessions),
        (2, 2)
    );
    assert!(!promotion.promotable());
    let text = calibrated.report.render();
    assert!(line_with(&text, "1. quality").ends_with(": pass"));
    assert!(line_with(&text, "2. cost:").ends_with(": fail"));
    assert!(line_with(&text, "3. latency:").ends_with(": pass"));
    assert!(line_with(&text, "quality.min_sessions 2").ends_with(": met"));
    assert!(line_with(&text, "promotion to live").contains("): no;"));
    assert!(line_with(&text, "M11 binding tests").ends_with(": fail"));
    assert!(!text.contains("all three"), "the unstaged verdict is gone");

    let tight = CalibrationConfig {
        latency_limit_ms: 10,
        ..config()
    };
    let calibrated = run_with(&tight, &[&one, &two]);
    assert_eq!(calibrated.report.promotion.latency.result, TestResult::Fail);
    assert!(line_with(&calibrated.report.render(), "3. latency:").ends_with(": fail"));
}

/// A learned candidate that differs from `rules` in shadow has no weight, so
/// no quality support: the quality tests say they cannot be evaluated rather
/// than inventing a number. Cost and latency are still read, from the
/// corrected quotes of the plan the learner chose (the owner's ruling of
/// 2026-09-29), on every interval.
#[test]
fn a_shadow_candidate_that_differs_from_rules_is_priced_from_quotes_but_not_quality_gated() {
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(
        corrected(Spec::new())
            .passing(Strategy::Efficient, 0.001)
            .choice(LearnedChoice::Exploit {
                strategy: Strategy::Efficient,
            })
            .decision(),
    );
    script.review(&[&turn], on_track());
    let calibrated = run(&[&script]);
    let learned = calibrated.report.estimate(Candidate::Learned).unwrap();
    assert_eq!(learned.weighted, 0);
    assert_eq!(calibrated.evidence.intervals[0].turns[0].learned, haiku());
    let promotion = calibrated.report.promotion;
    assert_eq!(promotion.agreeing, 0);
    for result in [
        promotion.quality_agreeing.result,
        promotion.quality_full.result,
    ] {
        assert!(matches!(result, TestResult::NotEvaluable(_)), "{result:?}");
    }
    assert_eq!(promotion.quality_full.learned_supported, 0);
    assert_eq!(promotion.cost.learned, CostEstimate::Priced(0.001));
    assert_eq!(promotion.cost.rules, CostEstimate::Priced(0.01));
    assert_eq!(promotion.cost.result, TestResult::Pass);
    assert_eq!(promotion.latency.p50_ms, Some(600));
    assert!(!promotion.promotable(), "no agreeing interval, no quality");
}

/// Every number that depends on the cluster unit carries its name, so a later
/// unit is one variant and not a rewrite of the report.
#[test]
fn the_cluster_unit_label_appears_on_every_clustered_number() {
    let one = shadow("acme/ada/one#g0", &[true, false]);
    let two = shadow("acme/ada/two#g0", &[true]);
    let text = run(&[&one, &two]).report.render();
    let unit = "sessions (sequence key)";
    for needle in [
        "cluster unit:",
        "of this project, from the source marks",
        "of other projects",
        "unreadable index mark",
        "after the manifest cutoff",
        "eligible intervals:",
        "bootstrap:",
        "bootstrap interval",
        "bootstrap lower bound",
        "weighted intervals:",
        "intervals rules served",
        "quality.min_sessions",
    ] {
        for line in text.lines().filter(|line| line.contains(needle)) {
            assert!(line.contains(unit), "no cluster unit on `{line}`");
        }
    }
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("bootstrap interval"))
            .count(),
        4,
        "one interval per candidate"
    );
}

#[test]
fn the_estimate_is_self_normalized_and_counts_its_effective_sample() {
    let mut weighted = outcome(0, true);
    weighted.weight = 4.0;
    let mut zero = outcome(1, false);
    zero.weight = 0.0;
    let estimate = estimate(
        &[weighted, outcome(2, false), zero],
        BootstrapPlan {
            seed: 1,
            resamples: 10,
        },
    );
    assert_eq!(estimate.snips, Some(0.8));
    assert_eq!(estimate.ips, 4.0 / 3.0);
    assert_eq!(estimate.effective_sample_size, 25.0 / 17.0);
    assert_eq!((estimate.weighted, estimate.weighted_clusters), (2, 2));
}

#[test]
fn a_candidate_action_is_its_strategys_first_target() {
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(Spec::new().decision());
    script.review(&[&turn], on_track());
    let calibrated = run(&[&script]);
    let facts = &calibrated.evidence.intervals[0].turns[0];
    let action = |candidate| facts.action(candidate).cloned();
    assert_eq!(action(Candidate::Fixed(Strategy::Efficient)), Some(haiku()));
    assert_eq!(action(Candidate::Fixed(Strategy::Capable)), Some(opus()));
    assert_eq!(
        action(Candidate::Learned),
        Some(opus()),
        "nothing passed: rules"
    );
    let _: Target = facts.served.clone();
}

/// A classifier result is spend once, against the intent it answers: a
/// repeated delivery and a result no intent asked for add nothing.
#[test]
fn classifier_spend_counts_each_call_once_against_its_intent() {
    let mut script = Script::named("acme/ada/one#g0");
    let turn = script.turn(Spec::new().decision());
    script.intent(&turn);
    let answer = classification(TurnComplexity::Routine, Some(TierChoice::Capable));
    script.answer(&turn, answer);
    script.answer(&turn, answer);
    let unasked = script.turn(Spec::new().decision());
    script.answer(&unasked, answer);
    script.review(&[&turn, &unasked], on_track());
    let spend = run(&[&script]).evidence.spend;
    let rules = spend
        .get(&roundhouse_core::routing::learn::offline::Stratum::Strategy(Strategy::Rules))
        .expect("the answered turn was served by rules");
    assert_eq!(rules.classifier_measured_calls, 1);
    assert_eq!(rules.classifier_measured_usd, 0.0005);
}
