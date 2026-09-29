// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The quality gate, `wilson-v1` (draft section 7.4, plan section 4).
//!
//! Three claims. The bounds are the Wilson score interval over credit units
//! scaled to intervals. The gate reads the most specific level whose live
//! evidence meets the minimum, and nothing else. And a prior, from the
//! calibration artifact or from Jev's tier answers, only ever shifts the
//! bounds of a level that live evidence already opened: it never counts toward
//! the minimums, so a prior alone gives `Unproven`, never `Pass` and never
//! `BelowFloor`.

mod learned_support;

use learned_support::*;
use roundhouse_core::routing::Tier;
use roundhouse_core::routing::learn::{
    Band, Bounds, GateResult, JEV_PRIOR_MIN_ANSWERS, JEV_PRIOR_PSEUDO_INTERVALS, JevCounts,
    KeyLevel, LearnedInput, LevelView, PriorBand, PriorSource, PriorUnits, QualityTerms, ReadView,
    Strategy, Units, jev_prior, read_gate, wilson_v1,
};

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

fn units(pos: u64, n: u64) -> Units {
    Units { pos, n }
}

/// The input every gate test reads: `rules` picked capable, no sequence.
fn input() -> LearnedInput {
    LearnedInput {
        rules_pick: Tier::Capable,
        newest: Band::None,
        prior: PriorBand::Absent,
        tool_turn: false,
    }
}

fn view(levels: Vec<LevelView>) -> ReadView {
    ReadView {
        levels,
        ..ReadView::default()
    }
}

fn at(level: KeyLevel, strategies: &[(Strategy, Counts)], jev: JevCounts) -> LevelView {
    learned_support::level(input().key(level), strategies, jev)
}

fn no_jev() -> JevCounts {
    JevCounts::default()
}

/// **Reference vectors computed outside this crate**, from the textbook Wilson
/// score formula over `n = units / 1000` (a short Python script, 2026-09-28),
/// so the test does not compare the implementation with itself.
#[test]
fn wilson_v1_bounds_match_fixed_vectors() {
    let vectors: [(u64, u64, f64, f64, f64); 8] = [
        (4_500, 5_000, 1.96, 0.462_936_442_589, 0.989_470_361_609),
        (18_000, 20_000, 1.96, 0.698_961_793_588, 0.972_134_106_016),
        (0, 5_000, 1.96, 0.0, 0.434_491_494_752),
        (5_000, 5_000, 1.96, 0.565_508_505_248, 1.0),
        (2_500, 3_000, 1.96, 0.309_981_541_765, 0.982_347_737_935),
        (7_500, 8_000, 1.96, 0.597_696_053_531, 0.993_440_279_397),
        (12_750, 15_000, 1.96, 0.602_319_987_271, 0.954_957_526_316),
        (7_500, 8_000, 2.576, 0.482_405_266_458, 0.995_874_805_606),
    ];
    for (pos, n, z, lower, upper) in vectors {
        let bounds = wilson_v1(units(pos, n), z);
        assert!(
            (bounds.lower - lower).abs() < 1e-9 && (bounds.upper - upper).abs() < 1e-9,
            "{pos}/{n} at z {z}: expected [{lower}, {upper}], got {bounds:?}"
        );
    }
    // No units at all is no knowledge: the whole interval.
    assert_eq!(
        wilson_v1(units(0, 0), 1.96),
        Bounds {
            lower: 0.0,
            upper: 1.0
        }
    );
}

/// The gate reads the most specific level whose *live* units meet the
/// minimum. A thin L2 does not hide a well-evidenced L1, and an evidenced L2
/// is read even when L1 says something else.
#[test]
fn the_gate_uses_the_most_specific_level_with_evidence() {
    let prior = PriorUnits::default();
    let quality = quality();
    let thin = (MIN_EVIDENCE - 1, MIN_EVIDENCE - 1, 20);

    let reading = |levels| {
        read_gate(
            &view(levels),
            &input(),
            Strategy::Efficient,
            &prior,
            &quality,
            true,
        )
    };

    // L2 one unit short, L1 passing, L0 below the floor: L1 decides.
    let first = reading(vec![
        at(KeyLevel::L2, &[(Strategy::Efficient, thin)], no_jev()),
        at(KeyLevel::L1, &[(Strategy::Efficient, PASS)], no_jev()),
        at(KeyLevel::L0, &[(Strategy::Efficient, BELOW)], no_jev()),
    ]);
    assert_eq!(first.level, Some(KeyLevel::L1));
    assert_eq!(first.result, GateResult::Pass);

    // L2 at the minimum and below the floor: L2 decides, whatever L1 says.
    let second = reading(vec![
        at(KeyLevel::L2, &[(Strategy::Efficient, BELOW)], no_jev()),
        at(KeyLevel::L1, &[(Strategy::Efficient, PASS)], no_jev()),
    ]);
    assert_eq!(second.level, Some(KeyLevel::L2));
    assert_eq!(second.result, GateResult::BelowFloor);

    // Another strategy's evidence at L2 is not this strategy's.
    let third = reading(vec![
        at(KeyLevel::L2, &[(Strategy::Capable, BELOW)], no_jev()),
        at(KeyLevel::L0, &[(Strategy::Efficient, PASS)], no_jev()),
    ]);
    assert_eq!(third.level, Some(KeyLevel::L0));
    assert_eq!(third.result, GateResult::Pass);

    // No level with evidence: unproven, and no level is recorded.
    let none = reading(vec![at(
        KeyLevel::L2,
        &[(Strategy::Efficient, thin)],
        no_jev(),
    )]);
    assert_eq!(none.level, None);
    assert_eq!(none.result, GateResult::Unproven);

    // A lower bound above the floor still needs the sessions.
    let few = reading(vec![at(
        KeyLevel::L1,
        &[(Strategy::Efficient, (PASS.0, PASS.1, MIN_SESSIONS - 1))],
        no_jev(),
    )]);
    assert_eq!(few.level, Some(KeyLevel::L1));
    assert_eq!(few.result, GateResult::Unproven);
}

/// An artifact prior is review evidence from another epoch. However large, it
/// opens no level: only live units count toward `min_evidence`.
#[test]
fn an_artifact_prior_alone_cannot_pass_the_gate() {
    let quality = quality();
    let huge = units(1_000_000, 1_000_000);
    let prior = PriorUnits::new(
        input()
            .keys()
            .into_iter()
            .map(|key| ((key, Strategy::Efficient), huge)),
    );
    let thin = (MIN_EVIDENCE - 1, MIN_EVIDENCE - 1, 1_000);
    for levels in [
        Vec::new(),
        vec![at(KeyLevel::L2, &[(Strategy::Efficient, thin)], no_jev())],
    ] {
        let reading = read_gate(
            &view(levels),
            &input(),
            Strategy::Efficient,
            &prior,
            &quality,
            true,
        );
        assert_eq!(reading.level, None);
        assert_eq!(reading.result, GateResult::Unproven);
    }
    // The same prior, all negative, cannot sink an unread level either.
    let negative = PriorUnits::new(
        input()
            .keys()
            .into_iter()
            .map(|key| ((key, Strategy::Efficient), units(0, 1_000_000))),
    );
    let reading = read_gate(
        &view(Vec::new()),
        &input(),
        Strategy::Efficient,
        &negative,
        &quality,
        true,
    );
    assert_eq!(reading.result, GateResult::Unproven);
}

/// Jev is a scout. A thousand agreeing answers on a key with no live evidence
/// leave every strategy unproven, and a thousand disagreeing ones do not sink
/// one below the floor.
#[test]
fn a_jev_prior_alone_cannot_pass_the_gate() {
    let quality = quality();
    let prior = PriorUnits::default();
    let thin = (MIN_EVIDENCE - 1, MIN_EVIDENCE - 1, 1_000);
    let agreeing = JevCounts {
        capable: 1_000,
        efficient: 0,
    };
    for live in [None, Some(thin)] {
        let strategies: Vec<(Strategy, Counts)> = live
            .map(|live| (Strategy::Capable, live))
            .into_iter()
            .collect();
        let levels = KeyLevel::ALL
            .into_iter()
            .map(|level| at(level, &strategies, agreeing))
            .collect();
        let view = view(levels);
        for strategy in [Strategy::Capable, Strategy::Efficient, Strategy::Rules] {
            let reading = read_gate(&view, &input(), strategy, &prior, &quality, true);
            assert_eq!(reading.level, None, "{strategy} with live {live:?}");
            assert_eq!(reading.result, GateResult::Unproven, "{strategy}");
        }
    }
}

/// The artifact prior wins where it holds anything; Jev's answers fill in only
/// where it holds nothing for that key and strategy.
#[test]
fn a_jev_prior_applies_only_where_the_artifact_prior_is_zero() {
    let quality = quality();
    let live = (15_000, 15_000, 20);
    let jev = JevCounts {
        capable: 0,
        efficient: 3,
    };
    let view = view(vec![at(KeyLevel::L2, &[(Strategy::Efficient, live)], jev)]);
    let l2 = input().key(KeyLevel::L2);

    let bare = read_gate(
        &view,
        &input(),
        Strategy::Efficient,
        &PriorUnits::default(),
        &quality,
        true,
    );
    assert_eq!(bare.prior_source, PriorSource::Jev);
    assert_eq!(bare.prior, units(3_000, 3_000));

    let artifact = units(500, 1_000);
    let calibrated = read_gate(
        &view,
        &input(),
        Strategy::Efficient,
        &PriorUnits::new([((l2, Strategy::Efficient), artifact)]),
        &quality,
        true,
    );
    assert_eq!(calibrated.prior_source, PriorSource::Artifact);
    assert_eq!(calibrated.prior, artifact);
    let expected = wilson_v1(units(15_500, 16_000), Z);
    assert!(close(calibrated.bounds.lower, expected.lower));
    assert!(close(calibrated.bounds.upper, expected.upper));

    // An artifact prior for another strategy, or at another key, leaves Jev's
    // prior in place for this one.
    let elsewhere = PriorUnits::new([
        ((l2, Strategy::Capable), artifact),
        ((input().key(KeyLevel::L0), Strategy::Efficient), artifact),
    ]);
    let other = read_gate(
        &view,
        &input(),
        Strategy::Efficient,
        &elsewhere,
        &quality,
        true,
    );
    assert_eq!(other.prior_source, PriorSource::Jev);

    // A turn that may not use Jev (a local-only pool) gets no prior from it.
    let barred = read_gate(
        &view,
        &input(),
        Strategy::Efficient,
        &PriorUnits::default(),
        &quality,
        false,
    );
    assert_eq!(barred.prior_source, PriorSource::None);
    assert_eq!(barred.prior, Units::default());
}

/// Fewer than three answers on a key is no prior. At three or more, the prior
/// is three intervals of `n`, split by agreement in integer arithmetic, and
/// the tier a strategy agrees with is the one it picks on that key.
#[test]
fn a_jev_prior_needs_the_minimum_answer_count() {
    assert_eq!(JEV_PRIOR_MIN_ANSWERS, 3);
    assert_eq!(JEV_PRIOR_PSEUDO_INTERVALS, 3);
    let two = JevCounts {
        capable: 2,
        efficient: 0,
    };
    assert_eq!(jev_prior(two, Tier::Capable), Units::default());
    let three = JevCounts {
        capable: 2,
        efficient: 1,
    };
    assert_eq!(jev_prior(three, Tier::Capable), units(2_000, 3_000));
    assert_eq!(jev_prior(three, Tier::Efficient), units(1_000, 3_000));
    // 3000 * 1 / 7 floors to 428.
    let seven = JevCounts {
        capable: 1,
        efficient: 6,
    };
    assert_eq!(jev_prior(seven, Tier::Capable), units(428, 3_000));

    // Through the gate: two answers leave the bounds on live units alone.
    let quality = quality();
    let live = (15_000, 15_000, 20);
    let bounds = |jev| {
        read_gate(
            &view(vec![at(KeyLevel::L2, &[(Strategy::Rules, live)], jev)]),
            &input(),
            Strategy::Rules,
            &PriorUnits::default(),
            &quality,
            true,
        )
    };
    let short = bounds(two);
    assert_eq!(short.prior_source, PriorSource::None);
    assert!(close(
        short.bounds.lower,
        wilson_v1(units(15_000, 15_000), Z).lower
    ));
    // `rules` picked capable on this key, so a capable answer agrees with it.
    let enough = bounds(JevCounts {
        capable: 3,
        efficient: 0,
    });
    assert_eq!(enough.prior, units(3_000, 3_000));
    assert!(close(
        enough.bounds.lower,
        wilson_v1(units(18_000, 18_000), Z).lower
    ));
}

/// Below `min_evidence` the prior is not read at all; at the minimum it joins
/// the live units. 15 live intervals, all positive, sit just under the floor
/// (lower bound 0.796); three agreeing pseudo-intervals lift it to 0.824.
#[test]
fn a_jev_prior_moves_the_bounds_only_after_live_evidence_meets_the_minimum() {
    let quality = QualityTerms {
        min_evidence: 15_000,
        ..quality()
    };
    let agree = JevCounts {
        capable: 3,
        efficient: 0,
    };
    let reading = |live: Counts, jev| {
        read_gate(
            &view(vec![at(KeyLevel::L2, &[(Strategy::Capable, live)], jev)]),
            &input(),
            Strategy::Capable,
            &PriorUnits::default(),
            &quality,
            true,
        )
    };

    let alone = reading((15_000, 15_000, 20), no_jev());
    assert_eq!(alone.result, GateResult::Unproven);
    let helped = reading((15_000, 15_000, 20), agree);
    assert_eq!(helped.result, GateResult::Pass);
    assert!(helped.bounds.lower > alone.bounds.lower);
    assert!(close(
        helped.bounds.lower,
        wilson_v1(units(18_000, 18_000), Z).lower
    ));

    // One unit short of the minimum: no level, whatever Jev says.
    let short = reading((14_999, 14_999, 20), agree);
    assert_eq!(short.level, None);
    assert_eq!(short.result, GateResult::Unproven);
    assert_eq!(
        short.bounds,
        Bounds {
            lower: 0.0,
            upper: 1.0
        }
    );
}
