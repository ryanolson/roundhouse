// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learned input and its three keys.
//!
//! The input is built from the classification window a decision records, so
//! every claim here is about what a decision could see at its cutoff: the
//! newest three classifications by the turn they describe, reduced to bands,
//! with the `rules` pick and the tool flag beside them. A key that ignored the
//! prior sequence, ordered by arrival, or read past the cutoff would count one
//! turn's evidence under another turn's key.

use roundhouse_core::classify::projection::PROJECTION_REVISION;
use roundhouse_core::classify::{
    AvailableClassification, ClassificationRef, ClassificationWindow, ContextDependence, Graded,
    TAXONOMY_VERSION, TurnClassification, TurnComplexity, TurnIntent,
};
use roundhouse_core::ids::ResponseId;
use roundhouse_core::routing::Tier;
use roundhouse_core::routing::learn::{Band, KeyLevel, LearnedInput, LevelKey, PriorBand};

/// A classification of `source_turn` that landed at `available_seq`.
fn classified(
    source_turn: u64,
    available_seq: u64,
    complexity: TurnComplexity,
) -> AvailableClassification {
    AvailableClassification {
        reference: ClassificationRef {
            call_id: ResponseId::new(format!("eval_{source_turn}_{available_seq}")),
            source_turn_index: source_turn,
            available_seq,
        },
        classification: TurnClassification {
            taxonomy_version: TAXONOMY_VERSION,
            intent: Graded {
                value: TurnIntent::Implement,
                confidence: 0.9,
            },
            complexity: Graded {
                value: complexity,
                confidence: 0.9,
            },
            context_dependence: Graded {
                value: ContextDependence::Recent,
                confidence: 0.9,
            },
            tier: None,
        },
    }
}

/// The window the engine records at `cutoff`: the newest `size` of the
/// classifications that had landed by then, in arrival order.
fn window_at(
    cutoff: u64,
    size: usize,
    available: &[AvailableClassification],
) -> ClassificationWindow {
    let landed: Vec<ClassificationRef> = available
        .iter()
        .filter(|entry| entry.reference.available_seq <= cutoff)
        .map(|entry| entry.reference.clone())
        .collect();
    ClassificationWindow::of(PROJECTION_REVISION, cutoff, size, landed.iter())
}

fn encode(available: &[AvailableClassification]) -> LearnedInput {
    let window = window_at(1_000, 8, available);
    LearnedInput::encode(Tier::Efficient, false, Some(&window), available)
}

/// **The claim.** The newest classification is the one about the latest turn,
/// not the one that arrived last, and the sequence keeps only the newest three.
#[test]
fn the_sequence_orders_by_source_turn_not_arrival() {
    // Arrival order: turn 5, then a delayed answer about turn 3, then turn 4.
    let available = [
        classified(5, 10, TurnComplexity::Deep),
        classified(3, 12, TurnComplexity::Routine),
        classified(4, 14, TurnComplexity::Trivial),
    ];
    let input = encode(&available);
    // By source turn: 3 low, 4 low, 5 high. By arrival the newest would be
    // turn 4 (low) and the prior would hold turn 5's high.
    assert_eq!(input.newest, Band::High);
    assert_eq!(input.prior, PriorBand::NoHigh);

    // A tie on source turn goes to the later arrival.
    let tied = [
        classified(5, 10, TurnComplexity::Deep),
        classified(5, 16, TurnComplexity::Routine),
    ];
    let input = encode(&tied);
    assert_eq!(input.newest, Band::Low);
    assert_eq!(input.prior, PriorBand::SomeHigh);

    // Only the newest three by source turn: turn 1's high is a fourth and
    // drops out, even though it is in the window.
    let four = [
        classified(1, 2, TurnComplexity::Involved),
        classified(3, 12, TurnComplexity::Routine),
        classified(4, 14, TurnComplexity::Trivial),
        classified(5, 16, TurnComplexity::Routine),
    ];
    let input = encode(&four);
    assert_eq!(input.newest, Band::Low);
    assert_eq!(input.prior, PriorBand::NoHigh);

    // A window capped below three gives a shorter sequence: one entry leaves
    // no prior at all.
    let window = window_at(1_000, 1, &four);
    let input = LearnedInput::encode(Tier::Efficient, false, Some(&window), &four);
    assert_eq!(input.newest, Band::Low);
    assert_eq!(input.prior, PriorBand::Absent);
}

/// **The claim.** Two turns whose newest classification agrees but whose
/// earlier ones differ are different L2 keys, and the same L1 and L0 key.
#[test]
fn identical_newest_labels_with_different_prior_sequences_give_different_l2_keys() {
    let after_a_hard_turn = encode(&[
        classified(2, 5, TurnComplexity::Deep),
        classified(3, 6, TurnComplexity::Routine),
    ]);
    let after_an_easy_turn = encode(&[
        classified(2, 5, TurnComplexity::Trivial),
        classified(3, 6, TurnComplexity::Routine),
    ]);
    assert_eq!(after_a_hard_turn.newest, after_an_easy_turn.newest);
    assert_ne!(
        after_a_hard_turn.key(KeyLevel::L2),
        after_an_easy_turn.key(KeyLevel::L2)
    );
    assert_ne!(
        after_a_hard_turn.key(KeyLevel::L2).part(),
        after_an_easy_turn.key(KeyLevel::L2).part(),
        "the store key part must differ too, or the counters merge"
    );
    assert_eq!(
        after_a_hard_turn.key(KeyLevel::L1),
        after_an_easy_turn.key(KeyLevel::L1)
    );
    assert_eq!(
        after_a_hard_turn.key(KeyLevel::L0),
        after_an_easy_turn.key(KeyLevel::L0)
    );
    // The tool flag is the fourth L2 field.
    let with_tools = LearnedInput {
        tool_turn: true,
        ..after_an_easy_turn
    };
    assert_ne!(
        with_tools.key(KeyLevel::L2),
        after_an_easy_turn.key(KeyLevel::L2)
    );
    assert_eq!(
        with_tools.key(KeyLevel::L1),
        after_an_easy_turn.key(KeyLevel::L1)
    );
}

/// **The claim.** A turn nobody classified is band `none`, which is neither
/// the classifier's `unknown` nor low complexity.
#[test]
fn no_available_classification_is_band_none_not_low() {
    // No classifier configured: no window at all.
    let unconfigured = LearnedInput::encode(Tier::Capable, true, None, &[]);
    assert_eq!(unconfigured.newest, Band::None);
    assert_eq!(unconfigured.prior, PriorBand::Absent);
    assert_eq!(unconfigured.key(KeyLevel::L1).part(), "capable.none");

    // A classifier configured and nothing landed yet.
    let empty = encode(&[]);
    assert_eq!(empty.newest, Band::None);
    assert_eq!(empty.prior, PriorBand::Absent);

    // Controls: the classifier's own `unknown`, and a low answer, are each
    // their own band.
    assert_eq!(
        encode(&[classified(1, 3, TurnComplexity::Unknown)]).newest,
        Band::Unknown
    );
    assert_eq!(
        encode(&[classified(1, 3, TurnComplexity::Trivial)]).newest,
        Band::Low
    );
    assert_ne!(
        empty.key(KeyLevel::L1),
        encode(&[classified(1, 3, TurnComplexity::Trivial)]).key(KeyLevel::L1)
    );
}

/// **The claim.** A classification that landed after the decision's cutoff is
/// not part of that decision's input, however it reaches the encoder.
#[test]
fn a_classification_after_the_cutoff_is_not_an_input() {
    let available = [
        classified(2, 15, TurnComplexity::Routine),
        // Landed at 25, after a cutoff of 20: during this very turn.
        classified(3, 25, TurnComplexity::Deep),
    ];
    // The engine's window at the cutoff does not name it, and the session's
    // accepted classifications, which do hold it, cannot bring it back.
    let window = window_at(20, 8, &available);
    let input = LearnedInput::encode(Tier::Efficient, false, Some(&window), &available);
    assert_eq!(input.newest, Band::Low);
    assert_eq!(input.prior, PriorBand::Absent);

    // A window that names it anyway is refused by its own cutoff.
    let mut malformed = window.clone();
    malformed.named.push(available[1].reference.clone());
    let input = LearnedInput::encode(Tier::Efficient, false, Some(&malformed), &available);
    assert_eq!(input.newest, Band::Low);
    assert_eq!(input.prior, PriorBand::Absent);

    // Control: at a later cutoff the same classification is the newest input.
    let later = window_at(30, 8, &available);
    let input = LearnedInput::encode(Tier::Efficient, false, Some(&later), &available);
    assert_eq!(input.newest, Band::High);
    assert_eq!(input.prior, PriorBand::NoHigh);
}

/// **The claim.** Every key level carries the `rules` pick, so evidence from
/// turns where `rules` picked efficient never lands on a key where it picked
/// capable.
#[test]
fn rules_pick_is_part_of_every_key_level() {
    let available = [
        classified(2, 5, TurnComplexity::Deep),
        classified(3, 6, TurnComplexity::Routine),
    ];
    let window = window_at(100, 8, &available);
    let efficient = LearnedInput::encode(Tier::Efficient, true, Some(&window), &available);
    let capable = LearnedInput::encode(Tier::Capable, true, Some(&window), &available);
    assert_eq!(efficient.rules_pick, Tier::Efficient);
    assert_eq!(capable.rules_pick, Tier::Capable);
    for level in KeyLevel::ALL {
        let (a, b) = (efficient.key(level), capable.key(level));
        assert_ne!(a, b, "{level:?}");
        assert_ne!(a.part(), b.part(), "{level:?}");
        assert_eq!(a.level(), level);
    }
    // The keys, most specific first, spelled as the store will spell them.
    assert_eq!(
        capable.keys().map(|key| key.to_string()),
        [
            "l2/capable.low.some_high.tools",
            "l1/capable.low",
            "l0/capable",
        ]
    );
    assert_eq!(
        efficient.key(KeyLevel::L0),
        LevelKey::L0 {
            rules_pick: Tier::Efficient
        }
    );
}
