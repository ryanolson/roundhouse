// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Learning entries folded from the log: existence, credit, operational rows,
//! and Jev counts (milestone M5 of `agent-docs/PLAN-online-routing-learner.md`,
//! draft sections 8 and 11.1). The cursor and the marks are in
//! `learning_cursor.rs`.

mod learning_support;

use roundhouse_core::classify::{TierChoice, TurnComplexity};
use roundhouse_core::routing::learn::{
    Band, CREDIT_SCALE, CacheReuse, JevCounts, LatencySum, LearnedInput, LevelKey, Strategy, Units,
};
use roundhouse_core::routing::{Target, Tier};
use roundhouse_core::session::{Deltas, LearningCauses, LearningEntry, TargetDelta};

use learning_support::*;

/// The quality units `entry` gives `strategy` at `key`, zero when none.
fn units(deltas: &Deltas, key: LevelKey, strategy: Strategy) -> Units {
    deltas
        .quality
        .iter()
        .find(|delta| delta.key == key && delta.strategy == strategy)
        .map_or(Units::default(), |delta| delta.units)
}

fn strategies_credited(deltas: &Deltas) -> Vec<Strategy> {
    let mut credited: Vec<Strategy> = deltas.quality.iter().map(|delta| delta.strategy).collect();
    credited.sort();
    credited.dedup();
    credited
}

fn target<'a>(deltas: &'a Deltas, target: &Target) -> Option<&'a TargetDelta> {
    let identity = target.policy_identity();
    deltas.targets.iter().find(|delta| delta.target == identity)
}

fn deltas(entry: &LearningEntry) -> &Deltas {
    entry
        .deltas
        .as_ref()
        .unwrap_or_else(|| panic!("entry at seq {} has no deltas", entry.seq))
}

fn keys(input: LearnedInput) -> [LevelKey; 3] {
    input.keys()
}

// ---- Existence -------------------------------------------------------------

/// **The event kind alone decides existence.** After the first learned
/// `Routed`, every `ValidationDecided`, terminal and `ClassificationRecorded`
/// is one entry: a rejected review, a validation that consulted nobody, a
/// duplicate classification and a turn that dispatched nothing all count,
/// with no deltas. Nothing before the first learned `Routed` does.
#[tokio::test]
async fn every_entry_producing_event_yields_one_entry_even_with_empty_deltas() {
    let mut script = Script::new();
    // Before learned evidence: a stage turn and a validation produce nothing.
    let early = script.turn(unlearned(opus()));
    script.not_run();
    script.intent(&early);
    script.answer(&early, classification(TurnComplexity::Routine, None));

    let learned = script.turn(Spec::new().decision());
    let mut expected = vec![learned.routed[0] + 2]; // its completion
    expected.push(script.not_run());
    // A review that recorded no coverage: it closes the interval and labels
    // nothing.
    expected.push(script.judged(None, on_track()));
    let refused = script.begin();
    expected.push(script.incomplete(&refused));
    script.intent(&learned);
    expected.push(script.answer(&learned, classification(TurnComplexity::Routine, None)));
    // Delivered twice: still an entry.
    expected.push(script.answer(&learned, classification(TurnComplexity::Routine, None)));
    // Not entry-producing, whatever follows learned evidence.
    script.intent(&refused);
    script.applied(0);
    // Nor is an acknowledgement, and the backfill from zero still holds every
    // entry it covered.
    script.applied(expected[2]);

    let entries = script.entries().await;
    let seqs: Vec<u64> = entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, expected, "one entry per entry-producing event");
    let empty = entries
        .iter()
        .filter(|entry| entry.deltas.is_none())
        .count();
    assert!(
        empty >= 4,
        "the not-run, uncovered, refused and duplicate events add nothing: {entries:?}"
    );
}

/// A newer build replaying an older session produces the same chain: which
/// entries exist does not depend on the credit revision a decision recorded.
#[tokio::test]
async fn entry_existence_does_not_depend_on_credit_revision() {
    async fn chain(revision: u32) -> Vec<LearningEntry> {
        let mut script = Script::new();
        let turn = script.turn(Spec::new().credit_revision(revision).decision());
        script.review(&[&turn], on_track());
        script.intent(&turn);
        script.answer(
            &turn,
            classification(TurnComplexity::Routine, Some(TierChoice::Capable)),
        );
        script.entries().await
    }
    let current = chain(roundhouse_core::routing::learn::LEARNING_CREDIT_REVISION).await;
    let foreign = chain(99).await;
    let links = |entries: &[LearningEntry]| {
        entries
            .iter()
            .map(|entry| (entry.seq, entry.prev_seq))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        current.len(),
        3,
        "completion, review and answer: {current:?}"
    );
    assert_eq!(links(&current), links(&foreign));
    assert!(
        current.iter().all(|entry| entry.deltas.is_some()),
        "{current:?}"
    );
    assert!(
        foreign.iter().all(|entry| entry.deltas.is_none()),
        "a foreign credit revision contributes no deltas: {foreign:?}"
    );
}

#[tokio::test]
async fn prev_seq_links_each_entry_to_the_previous_one() {
    let mut script = Script::new();
    let first = script.turn(Spec::new().decision());
    script.not_run();
    let second = script.turn(Spec::new().decision());
    script.review(&[&first, &second], on_track());

    let entries = script.entries().await;
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert_eq!(entries[0].prev_seq, 0, "the first entry links to nothing");
    for pair in entries.windows(2) {
        assert_eq!(pair[1].prev_seq, pair[0].seq, "{entries:?}");
    }
}

// ---- Credit ----------------------------------------------------------------

/// **One interval is one unit per strategy per level**, split over the keys
/// it visited in proportion to their turns, the remainder to the key that
/// appeared first. `rules` and `capable` planned the served target on every
/// turn; `efficient` never did.
#[tokio::test]
async fn a_consistent_strategy_receives_one_interval_unit_split_over_keys() {
    let low = input(Tier::Capable, Band::Low);
    let high = input(Tier::Capable, Band::High);
    let mut script = Script::new();
    let a = script.turn(Spec::new().input(low).decision());
    let b = script.turn(Spec::new().input(high).decision());
    let c = script.turn(Spec::new().input(low).decision());
    let review = script.review(&[&a, &b, &c], on_track());

    let entry = script.entry(review).await;
    let deltas = deltas(&entry);
    assert_eq!(deltas.epoch, epoch());
    assert_eq!(
        strategies_credited(deltas),
        vec![Strategy::Rules, Strategy::Capable]
    );
    for strategy in [Strategy::Rules, Strategy::Capable] {
        for level in 0..2 {
            let (low_key, high_key) = (keys(low)[level], keys(high)[level]);
            assert_eq!(
                units(deltas, low_key, strategy),
                Units { pos: 667, n: 667 },
                "two of three turns, plus the remainder: {deltas:?}"
            );
            assert_eq!(
                units(deltas, high_key, strategy),
                Units { pos: 333, n: 333 }
            );
        }
        assert_eq!(
            units(deltas, keys(low)[2], strategy),
            Units {
                pos: CREDIT_SCALE,
                n: CREDIT_SCALE
            },
            "one L0 key holds the whole unit"
        );
    }
    let total: u64 = deltas.quality.iter().map(|delta| delta.units.n).sum();
    assert_eq!(
        total,
        2 * 3 * CREDIT_SCALE,
        "one unit per strategy and level"
    );
}

/// A strategy whose plan missed the served target on one covered turn gets
/// nothing for the interval, and a `Negative` label adds `n` only.
#[tokio::test]
async fn an_inconsistent_strategy_receives_nothing() {
    let mut script = Script::new();
    let agrees = vec![
        (Strategy::Rules, haiku()),
        (Strategy::Efficient, haiku()),
        (Strategy::Capable, opus()),
    ];
    let a = script.turn(Spec::new().chosen(haiku()).plans(agrees.clone()).decision());
    let b = script.turn(Spec::new().chosen(haiku()).plans(agrees).decision());
    // Here `rules` escalated and served opus; `efficient` still planned haiku.
    let c = script.turn(Spec::new().decision());
    let review = script.review(&[&a, &b, &c], off_track());

    let entry = script.entry(review).await;
    let deltas = deltas(&entry);
    assert_eq!(strategies_credited(deltas), vec![Strategy::Rules]);
    let key = Spec::new().input.keys()[2];
    assert_eq!(
        units(deltas, key, Strategy::Rules),
        Units {
            pos: 0,
            n: CREDIT_SCALE
        },
        "a negative interval adds to n only"
    );
}

#[tokio::test]
async fn a_failover_in_the_interval_credits_nothing() {
    let mut script = Script::new();
    let clean = script.turn(Spec::new().decision());
    let mut failed = script.begin();
    script.route(&mut failed, Spec::new().chosen(haiku()).decision());
    script.route(&mut failed, Spec::new().failed_before().decision());
    script.complete(&failed, unmeasured());
    let review = script.review(&[&clean, &failed], on_track());

    let state = script.state().await;
    assert_eq!(state.accepted_reviews(), 1, "the review itself is sound");
    assert_eq!(script.entry(review).await.deltas, None);
    assert_eq!(state.learning_causes().failover_in_interval, 1);
}

#[tokio::test]
async fn another_epoch_or_credit_revision_credits_nothing() {
    let mut script = Script::new();
    let a = script.turn(Spec::new().decision());
    let b = script.turn(Spec::new().epoch(other_epoch()).decision());
    let mixed = script.review(&[&a, &b], on_track());
    let c = script.turn(Spec::new().credit_revision(99).decision());
    let foreign = script.review(&[&c], on_track());
    let d = script.turn(Spec::new().decision());
    let e = script.turn(unlearned(opus()));
    let missing = script.review(&[&d, &e], on_track());

    let state = script.state().await;
    assert_eq!(state.accepted_reviews(), 3);
    for seq in [mixed, foreign, missing] {
        assert_eq!(script.entry(seq).await.deltas, None, "review at {seq}");
    }
    let causes = state.learning_causes();
    assert_eq!(
        (
            causes.mixed_epoch,
            causes.other_credit_revision,
            causes.missing_row
        ),
        (1, 1, 1),
        "{causes:?}"
    );
}

/// Each cause alone, so the counters cannot trade places: the combined test
/// above sets one of each and would pass with any two swapped.
#[tokio::test]
async fn a_mixed_epoch_review_counts_only_the_epoch_cause() {
    let mut script = Script::new();
    let a = script.turn(Spec::new().decision());
    let b = script.turn(Spec::new().epoch(other_epoch()).decision());
    script.review(&[&a, &b], on_track());

    let state = script.state().await;
    assert_eq!(state.accepted_reviews(), 1);
    assert_eq!(
        state.learning_causes(),
        LearningCauses {
            mixed_epoch: 1,
            ..LearningCauses::default()
        }
    );
}

#[tokio::test]
async fn a_foreign_credit_revision_review_counts_only_the_revision_cause() {
    let mut script = Script::new();
    let turn = script.turn(Spec::new().credit_revision(99).decision());
    script.review(&[&turn], on_track());

    let state = script.state().await;
    assert_eq!(state.accepted_reviews(), 1);
    assert_eq!(
        state.learning_causes(),
        LearningCauses {
            other_credit_revision: 1,
            ..LearningCauses::default()
        }
    );
}

#[tokio::test]
async fn an_unknown_label_yields_an_entry_without_quality_deltas() {
    let mut script = Script::new();
    let turn = script.turn(Spec::new().decision());
    let review = script.review(&[&turn], blind());

    let state = script.state().await;
    assert_eq!(state.accepted_reviews(), 1);
    assert_eq!(script.entry(review).await.deltas, None);
    assert_eq!(state.learning_causes().unknown_label, 1);
}

/// An infeasible `live` turn served `rules` without the learner validating the
/// route. The review of what it served is still real, so the strategies whose
/// plans matched earn it.
#[tokio::test]
async fn a_serve_rules_turn_still_produces_credit_for_consistent_strategies() {
    let mut script = Script::new();
    let turn = script.turn(Spec::new().live().decision());
    let review = script.review(&[&turn], on_track());

    let entry = script.entry(review).await;
    let deltas = deltas(&entry);
    assert_eq!(
        strategies_credited(deltas),
        vec![Strategy::Rules, Strategy::Capable]
    );
}

/// **Credit reads the keys the decision recorded.** A classification of turn 0
/// that lands later would change a recomputed input, but the review credits
/// the key turn 0 was decided under, and entries already folded do not move.
#[tokio::test]
async fn a_late_review_does_not_change_recorded_inputs() {
    let recorded = input(Tier::Capable, Band::None);
    let later = input(Tier::Capable, Band::High);
    let mut script = Script::new();
    let first = script.turn(Spec::new().input(recorded).decision());
    script.intent(&first);
    script.answer(&first, classification(TurnComplexity::Deep, None));
    let second = script.turn(Spec::new().input(later).decision());
    let before = script.entries().await;
    let review = script.review(&[&first, &second], on_track());

    let after = script.entries().await;
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "folded entries never move"
    );
    let entry = script.entry(review).await;
    let deltas = deltas(&entry);
    assert_eq!(
        units(deltas, recorded.keys()[0], Strategy::Rules),
        Units { pos: 500, n: 500 },
        "turn 0 is credited under the key it recorded: {deltas:?}"
    );
    assert_eq!(
        units(deltas, later.keys()[0], Strategy::Rules),
        Units { pos: 500, n: 500 }
    );
}

// ---- Operational rows -------------------------------------------------------

/// One turn with a slow start: routed three seconds after it started, first
/// output 500 ms after the dispatch, on a 200 ms quote.
fn slow_start(script: &mut Script) -> u64 {
    let mut turn = script.begin_at(10_000);
    script.route_at(&mut turn, 13_000, Spec::new().decision());
    script.delta_at(&turn, 13_500, "hello");
    script.complete_at(&turn, 14_000, unmeasured())
}

#[tokio::test]
async fn the_latency_interval_starts_at_the_served_routed_event() {
    let mut script = Script::new();
    let done = slow_start(&mut script);

    let entry = script.entry(done).await;
    let row = target(deltas(&entry), &opus()).expect("a row for the served target");
    assert_eq!(
        row.latency,
        LatencySum { sum_ms: 300, n: 1 },
        "500 ms from the dispatch less the 200 ms quote, not from turn start"
    );
}

/// The overhead runs from turn start to the *served* dispatch, so a failed
/// attempt's time is overhead; the failed target gets the failover count and
/// the served one the residual.
#[tokio::test]
async fn the_overhead_sample_spans_turn_start_to_the_served_routed() {
    let mut script = Script::new();
    let done = slow_start(&mut script);
    let mut failed = script.begin_at(20_000);
    script.route_at(&mut failed, 20_100, Spec::new().chosen(haiku()).decision());
    script.route_at(&mut failed, 23_000, Spec::new().failed_before().decision());
    script.delta_at(&failed, 23_400, "hello");
    let failed_done = script.complete_at(&failed, 24_000, unmeasured());

    let entry = script.entry(done).await;
    assert_eq!(
        deltas(&entry).overhead,
        LatencySum {
            sum_ms: 3_000,
            n: 1
        }
    );
    let entry = script.entry(failed_done).await;
    let deltas = deltas(&entry);
    assert_eq!(
        deltas.overhead,
        LatencySum {
            sum_ms: 3_000,
            n: 1
        }
    );
    let first = target(deltas, &haiku()).expect("the first dispatched target");
    assert_eq!((first.failover, first.latency.n), (1, 0));
    let served = target(deltas, &opus()).expect("the served target");
    assert_eq!(
        (served.failover, served.latency),
        (0, LatencySum { sum_ms: 200, n: 1 })
    );
}

#[tokio::test]
async fn overhead_and_residual_samples_come_from_the_same_turns() {
    let mut script = Script::new();
    slow_start(&mut script);
    // Completed without output: neither sample.
    let mut silent = script.begin();
    script.route(&mut silent, Spec::new().decision());
    script.complete(&silent, unmeasured());
    // Output, then no completion: neither sample.
    let mut broken = script.begin();
    script.route(&mut broken, Spec::new().decision());
    let at = script.clock + 10;
    script.delta_at(&broken, at, "partial");
    script.incomplete(&broken);

    let entries = script.entries().await;
    let (mut residuals, mut overheads) = (Vec::new(), Vec::new());
    for entry in &entries {
        if let Some(deltas) = &entry.deltas {
            if deltas.targets.iter().any(|row| row.latency.n > 0) {
                residuals.push(entry.seq);
            }
            if deltas.overhead.n > 0 {
                overheads.push(entry.seq);
            }
        }
    }
    assert_eq!(residuals.len(), 1, "{entries:?}");
    assert_eq!(residuals, overheads, "both samples, from the same turn");
}

/// A review turn runs the judge between `TurnStarted` and `Routed`: that time
/// is overhead, and the residual of the target is untouched by it.
#[tokio::test]
async fn a_review_turn_judge_call_enters_the_overhead_and_not_the_residual() {
    let mut script = Script::new();
    let mut turn = script.begin_at(10_000);
    script.push_at(
        12_000,
        roundhouse_core::event::SessionEventKind::ValidationDecided {
            validation_id: roundhouse_core::ids::ValidationId::new("judge"),
            trigger: roundhouse_core::validate::TriggerRecord::new(0, 0, Vec::new()),
            arm: roundhouse_core::validate::Arm::Shadow,
            outcome: roundhouse_core::event::ValidationOutcome::NotRun {
                reason: roundhouse_core::event::NotRunReason::JudgeFailed,
            },
        },
    );
    script.route_at(&mut turn, 12_100, Spec::new().decision());
    script.delta_at(&turn, 12_400, "hello");
    let done = script.complete_at(&turn, 12_500, unmeasured());

    let entry = script.entry(done).await;
    let deltas = deltas(&entry);
    assert_eq!(
        deltas.overhead,
        LatencySum {
            sum_ms: 2_100,
            n: 1
        }
    );
    assert_eq!(
        target(deltas, &opus()).map(|row| row.latency),
        Some(LatencySum { sum_ms: 100, n: 1 })
    );
}

/// Missing output is not zero latency.
#[tokio::test]
async fn a_turn_without_first_output_supplies_no_latency_sample() {
    let mut script = Script::new();
    let mut quiet = script.begin();
    script.route(&mut quiet, Spec::new().decision());
    let at = script.clock + 10;
    script.delta_at(&quiet, at, "");
    let quiet_done = script.complete(&quiet, measured(600));
    let mut failed = script.begin();
    script.route(&mut failed, Spec::new().decision());
    let at = script.clock + 10;
    script.delta_at(&failed, at, "partial");
    let failed_done = script.incomplete(&failed);

    for seq in [quiet_done, failed_done] {
        let entry = script.entry(seq).await;
        if let Some(deltas) = &entry.deltas {
            assert_eq!(deltas.overhead.n, 0, "{entry:?}");
            assert!(
                deltas.targets.iter().all(|row| row.latency.n == 0),
                "{entry:?}"
            );
        }
    }
    // The quiet turn still measured its cache: the entry is not empty.
    assert!(script.entry(quiet_done).await.deltas.is_some());
}

/// Only a served frontier dispatch whose provider stated its cache read adds a
/// reuse sample, in per-mille of the prompt.
#[tokio::test]
async fn cache_rows_require_provider_measurement() {
    let mut script = Script::new();
    let mut measured_turn = script.begin();
    script.route(&mut measured_turn, Spec::new().decision());
    let measured_done = script.complete(&measured_turn, measured(450));
    let mut unmeasured_turn = script.begin();
    script.route(&mut unmeasured_turn, Spec::new().decision());
    let unmeasured_done = script.complete(&unmeasured_turn, unmeasured());
    let local_plans = vec![(Strategy::Rules, qwen()), (Strategy::Efficient, qwen())];
    let mut local_turn = script.begin();
    script.route(
        &mut local_turn,
        Spec::new().chosen(qwen()).plans(local_plans).decision(),
    );
    let local_done = script.complete(&local_turn, measured(450));
    // 4506 of 10000 is 450.6 per mille: 451 to nearest, 450 truncated. Nearest
    // is unbiased, so a sum of many samples keeps the ratio of the sums.
    let mut fractional_turn = script.begin();
    script.route(&mut fractional_turn, Spec::new().decision());
    let fractional_done = script.complete(
        &fractional_turn,
        roundhouse_core::event::Usage {
            input_tokens: 10_000,
            cached_input_tokens: 4_506,
            ..measured(0)
        },
    );

    let entry = script.entry(measured_done).await;
    assert_eq!(
        target(deltas(&entry), &opus()).map(|row| row.cache),
        Some(CacheReuse {
            predicted_permille: 600,
            observed_permille: 450,
            n: 1
        })
    );
    let entry = script.entry(fractional_done).await;
    assert_eq!(
        target(deltas(&entry), &opus()).map(|row| row.cache.observed_permille),
        Some(451),
        "per mille rounds to nearest"
    );
    for seq in [unmeasured_done, local_done] {
        let entry = script.entry(seq).await;
        let cached = entry
            .deltas
            .iter()
            .flat_map(|deltas| deltas.targets.iter())
            .map(|row| row.cache.n)
            .sum::<u64>();
        assert_eq!(cached, 0, "{entry:?}");
    }
}

// ---- Jev counts ---------------------------------------------------------------

/// A tier answer is one count on each of the three keys of the turn it is
/// about, and never touches the review counts.
#[tokio::test]
async fn a_classification_with_a_tier_answer_adds_one_jev_count_on_each_key_of_its_source_turn() {
    let decided = input(Tier::Efficient, Band::Low);
    let mut script = Script::new();
    let turn = script.turn(Spec::new().input(decided).decision());
    script.intent(&turn);
    let answer = script.answer(
        &turn,
        classification(TurnComplexity::Deep, Some(TierChoice::Capable)),
    );

    let entry = script.entry(answer).await;
    let deltas = deltas(&entry);
    assert_eq!(deltas.epoch, epoch());
    let counted: Vec<(LevelKey, JevCounts)> = deltas
        .jev
        .iter()
        .map(|delta| (delta.key, delta.counts))
        .collect();
    let one = JevCounts {
        capable: 1,
        efficient: 0,
    };
    assert_eq!(
        counted,
        decided.keys().map(|key| (key, one)).to_vec(),
        "the keys the source turn recorded, not the answer's own band"
    );
    assert!(deltas.quality.is_empty(), "a scout is never a reward");
    assert!(deltas.targets.is_empty() && deltas.overhead.n == 0);
}

#[tokio::test]
async fn a_classification_without_a_tier_answer_yields_an_entry_with_no_deltas() {
    let mut script = Script::new();
    let turn = script.turn(Spec::new().decision());
    script.intent(&turn);
    let answer = script.answer(&turn, classification(TurnComplexity::Routine, None));

    assert_eq!(script.entry(answer).await.deltas, None);
}

#[tokio::test]
async fn a_classification_for_a_turn_without_a_learned_row_yields_an_entry_with_no_deltas() {
    let mut script = Script::new();
    script.turn(Spec::new().decision());
    let stage = script.turn(unlearned(opus()));
    script.intent(&stage);
    let answer = script.answer(
        &stage,
        classification(TurnComplexity::Routine, Some(TierChoice::Efficient)),
    );

    assert_eq!(script.entry(answer).await.deltas, None);
}

/// The row waits from the intent to the result, however long the result takes
/// to be delivered, and is dropped once a later intent is requested after the
/// first one expired.
#[tokio::test]
async fn the_learning_row_is_kept_from_the_intent_until_the_result_or_expiry() {
    let tier = || classification(TurnComplexity::Routine, Some(TierChoice::Efficient));
    let mut script = Script::new();
    let first = script.turn(Spec::new().decision());
    script.intent_at(&first, 100_000, 130_000);
    // Delivered at the start of a turn an idle hour later.
    let late = script.answer_at(&first, 3_700_000, tier());

    let second = script.turn(Spec::new().decision());
    script.intent_at(&second, 4_000_000, 4_030_000);
    let third = script.turn(Spec::new().decision());
    // Requested after the second intent expired: its row is gone.
    script.intent_at(&third, 5_000_000, 5_030_000);
    let expired = script.answer_at(&second, 5_000_100, tier());
    let answered = script.answer_at(&third, 5_000_200, tier());

    assert!(
        !deltas(&script.entry(late).await).jev.is_empty(),
        "an answer delivered long after its call's deadline still counts"
    );
    assert_eq!(script.entry(expired).await.deltas, None);
    assert!(!deltas(&script.entry(answered).await).jev.is_empty());
}
