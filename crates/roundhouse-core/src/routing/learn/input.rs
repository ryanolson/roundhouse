// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learned input of one turn and the three keys it is counted under.
//!
//! Revision [`LEARNING_INPUT_REVISION`](super::LEARNING_INPUT_REVISION) 1:
//! `rules_pick`, the complexity band of the newest classification, a summary
//! of the older ones, and whether the client declared tools. Every field
//! affects selection, and nothing that affects selection is left out: input
//! size and cache state reach a strategy through the quotes of its plan, not
//! through the key.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::classify::{AvailableClassification, ClassificationWindow, TurnComplexity};
use crate::routing::stage::Tier;

/// How many classifications the sequence holds at most (`K`).
pub const SEQUENCE_LEN: usize = 3;

/// One classification's complexity, reduced to what the key distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// No classification in that position.
    ///
    /// **Not `unknown` and not `low`.** Nothing was said about the turn: the
    /// classifier was not configured, had not answered yet, or answered after
    /// the cutoff. Folding that into `low` would count a session nobody
    /// classified as a session of easy turns.
    None,
    /// The classifier answered `unknown`.
    Unknown,
    /// `trivial` or `routine`.
    Low,
    /// `involved` or `deep`.
    High,
}

impl Band {
    pub fn of(complexity: TurnComplexity) -> Self {
        match complexity {
            TurnComplexity::Trivial | TurnComplexity::Routine => Band::Low,
            TurnComplexity::Involved | TurnComplexity::Deep => Band::High,
            TurnComplexity::Unknown => Band::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Band::None => "none",
            Band::Unknown => "unknown",
            Band::Low => "low",
            Band::High => "high",
        }
    }
}

/// The older classifications of the sequence, summarized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorBand {
    /// The sequence holds fewer than two classifications.
    Absent,
    /// None of the older ones is `high`.
    NoHigh,
    /// At least one of the older ones is `high`.
    SomeHigh,
}

impl PriorBand {
    pub fn label(self) -> &'static str {
        match self {
            PriorBand::Absent => "absent",
            PriorBand::NoHigh => "no_high",
            PriorBand::SomeHigh => "some_high",
        }
    }
}

/// One turn's learned input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LearnedInput {
    /// The tier the `rules` strategy *picked*, before any cost guard or empty
    /// tier moved the served target.
    ///
    /// **The pick, not the served tier.** The key exists to separate turns on
    /// which `rules` and a fixed-tier strategy agree from turns on which they
    /// disagree, and that is a fact about picks. A cost-guarded turn picked
    /// efficient and served capable; keying it as capable would mix it into the
    /// turns where `rules` escalated.
    pub rules_pick: Tier,
    pub newest: Band,
    pub prior: PriorBand,
    /// The client declared tools on this turn.
    pub tool_turn: bool,
}

impl LearnedInput {
    /// The input for one turn, from what the decision could see.
    ///
    /// **The sequence comes from the recorded window, not from the session's
    /// whole history.** `window` is the [`ClassificationWindow`] the decision
    /// records on its `Routed`, so the input is exactly reconstructable from
    /// the log; a window capped below [`SEQUENCE_LEN`] gives a shorter
    /// sequence, and a deployment with no classifier has no window and so no
    /// sequence. `available` is where the named references are resolved to
    /// their answers — the session's accepted classifications.
    ///
    /// **A named reference above the window's cutoff is not an input**, even
    /// though a well-formed window never names one: the cutoff is the rule
    /// that stops a result which landed during this turn from backdating it,
    /// and it is checked here rather than trusted.
    ///
    /// **Ordered by source turn, not by arrival.** A delayed answer about turn
    /// 3 can land after the answer about turn 5; the newest classification is
    /// the one about the latest turn. A tie goes to the later arrival.
    pub fn encode(
        rules_pick: Tier,
        tool_turn: bool,
        window: Option<&ClassificationWindow>,
        available: &[AvailableClassification],
    ) -> Self {
        let mut sequence: Vec<&AvailableClassification> = window
            .into_iter()
            .flat_map(|window| {
                window
                    .named
                    .iter()
                    .filter(move |named| named.available_seq <= window.cutoff_seq)
            })
            .filter_map(|named| {
                // From the newest end: a window names the newest references, so
                // a reverse scan finds each one after a few steps rather than
                // walking the whole session history.
                available
                    .iter()
                    .rev()
                    .find(|entry| entry.reference == *named)
            })
            .collect();
        sequence.sort_by_key(|entry| {
            (
                entry.reference.source_turn_index,
                entry.reference.available_seq,
            )
        });
        let sequence = &sequence[sequence.len().saturating_sub(SEQUENCE_LEN)..];
        let band =
            |entry: &&AvailableClassification| Band::of(entry.classification.complexity.value);

        let (newest, older) = match sequence.split_last() {
            Some((newest, older)) => (band(newest), older),
            None => (Band::None, sequence),
        };
        let prior = match older {
            [] => PriorBand::Absent,
            older if older.iter().any(|entry| band(entry) == Band::High) => PriorBand::SomeHigh,
            _ => PriorBand::NoHigh,
        };
        Self {
            rules_pick,
            newest,
            prior,
            tool_turn,
        }
    }

    /// The key at one level.
    pub fn key(&self, level: KeyLevel) -> LevelKey {
        let Self {
            rules_pick,
            newest,
            prior,
            tool_turn,
        } = *self;
        match level {
            KeyLevel::L2 => LevelKey::L2 {
                rules_pick,
                newest,
                prior,
                tool_turn,
            },
            KeyLevel::L1 => LevelKey::L1 { rules_pick, newest },
            KeyLevel::L0 => LevelKey::L0 { rules_pick },
        }
    }

    /// The three keys, most specific first.
    pub fn keys(&self) -> [LevelKey; 3] {
        KeyLevel::ALL.map(|level| self.key(level))
    }
}

/// How specific a key is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyLevel {
    /// `rules_pick`, `newest`, `prior`, `tool_turn`: 48 keys.
    L2,
    /// `rules_pick`, `newest`: 8 keys.
    L1,
    /// `rules_pick`: 2 keys.
    L0,
}

impl KeyLevel {
    /// Most specific first, the order the gate reads them in.
    pub const ALL: [KeyLevel; 3] = [KeyLevel::L2, KeyLevel::L1, KeyLevel::L0];

    pub fn label(self) -> &'static str {
        match self {
            KeyLevel::L2 => "l2",
            KeyLevel::L1 => "l1",
            KeyLevel::L0 => "l0",
        }
    }
}

/// One key of the learned state.
///
/// **`rules_pick` is in every level.** Within one key `rules` then picks one
/// tier, so whether a fixed-tier strategy agrees with it is nearly constant
/// across the key. Without it, `efficient` in a key would earn its evidence on
/// the turns where `rules` also picked efficient — the easier part of the key —
/// and the gate could pass it on the harder part.
///
/// An enum rather than one struct with optional fields, so an `l0` key that
/// carries a band is not a value anyone can build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum LevelKey {
    L2 {
        rules_pick: Tier,
        newest: Band,
        prior: PriorBand,
        tool_turn: bool,
    },
    L1 {
        rules_pick: Tier,
        newest: Band,
    },
    L0 {
        rules_pick: Tier,
    },
}

impl LevelKey {
    pub fn level(&self) -> KeyLevel {
        match self {
            LevelKey::L2 { .. } => KeyLevel::L2,
            LevelKey::L1 { .. } => KeyLevel::L1,
            LevelKey::L0 { .. } => KeyLevel::L0,
        }
    }

    /// The key without its level, as one store key part.
    ///
    /// **Dot-separated and colon-free**, because the store joins key parts
    /// with `:` and the level is its own part. The tier is spelled
    /// `capable`/`efficient`, the configuration's words, and not the
    /// `strong`/`weak` labels a rationale uses: a key is an identifier, and an
    /// identifier that changed with a display label would orphan its counters.
    pub fn part(&self) -> String {
        fn tier(tier: Tier) -> &'static str {
            match tier {
                Tier::Capable => "capable",
                Tier::Efficient => "efficient",
            }
        }
        match *self {
            LevelKey::L2 {
                rules_pick,
                newest,
                prior,
                tool_turn,
            } => format!(
                "{}.{}.{}.{}",
                tier(rules_pick),
                newest.label(),
                prior.label(),
                match tool_turn {
                    true => "tools",
                    false => "no_tools",
                }
            ),
            LevelKey::L1 { rules_pick, newest } => {
                format!("{}.{}", tier(rules_pick), newest.label())
            }
            LevelKey::L0 { rules_pick } => tier(rules_pick).to_string(),
        }
    }
}

impl fmt::Display for LevelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.level().label(), self.part())
    }
}
