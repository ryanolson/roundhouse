// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The calibration artifact a project's learner starts from, and the epoch id
//! it names (draft sections 6 and 14.5).
//!
//! **Parsed here, read from disk by the caller.** The server's configuration
//! loader reads the file and hands the bytes over; the offline calibrator
//! (milestone M10) writes the same format. One parser for both is what keeps
//! a calibrator's output loadable by the process that serves it.
//!
//! **The epoch hashes the artifact's bytes, not its parsed value.** Two
//! artifacts that parse to the same prior but differ by a byte are two
//! epochs. The calibrator writes byte-identical output for identical input
//! (draft 14.5), so an unchanged calibration keeps its epoch, and any edit,
//! even whitespace, starts a new one rather than silently reusing counters
//! learned under the old file.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::input::LevelKey;
use super::{
    EpochId, LEARNING_CREDIT_REVISION, LEARNING_INPUT_REVISION, PriorUnits, Strategy, StrategySet,
    Units,
};
use crate::routing::selection::{LEARNED_SELECTOR_REVISION, STAGE_SELECTOR_REVISION};

/// The artifact format this build reads.
pub const ARTIFACT_SCHEMA_REVISION: u32 = 1;

/// The only gate this build implements.
pub const GATE_NAME: &str = "wilson-v1";

/// A parsed, checked calibration artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    strategies: StrategySet,
    prior: PriorUnits,
    epoch: EpochId,
    manifest_digest: String,
    source_commit: String,
}

/// The artifact as written. Every field is required and no unknown field is
/// accepted: an artifact that means less than it says would give a project a
/// prior nobody calibrated.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactFile {
    schema_revision: u32,
    input_revision: u32,
    selector_revision: u32,
    stage_revision: u32,
    credit_revision: u32,
    gate: String,
    strategies: Vec<Strategy>,
    prior: Vec<PriorEntry>,
    manifest_digest: String,
    source_commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorEntry {
    key: LevelKey,
    strategy: Strategy,
    pos: u64,
    n: u64,
}

/// Why an artifact was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("the artifact is not valid JSON of the artifact format: {0}")]
    Format(String),
    #[error("schema revision {found} is not {ARTIFACT_SCHEMA_REVISION}, the one this build reads")]
    Schema { found: u32 },
    /// The artifact was calibrated under another input, learned selector,
    /// stage selector or credit revision, so its prior counts turns this
    /// build would key or credit differently.
    #[error("the artifact's {which} revision {found} is not this build's {expected}")]
    Revision {
        which: &'static str,
        found: u32,
        expected: u32,
    },
    #[error("the artifact's gate `{0}` is not `{GATE_NAME}`")]
    Gate(String),
    #[error("the artifact's strategy list is refused: {0}")]
    Strategies(String),
    #[error("the artifact lists prior units for `{key}` and `{strategy}` twice")]
    RepeatedPrior { key: LevelKey, strategy: Strategy },
    #[error("the artifact's prior for `{key}` and `{strategy}` has pos {pos} above n {n}")]
    PriorRange {
        key: LevelKey,
        strategy: Strategy,
        pos: u64,
        n: u64,
    },
    /// Prior units for a strategy the artifact does not list: a prior nothing
    /// would read, which says the artifact and its list disagree.
    #[error("the artifact holds a prior for `{0}`, which its strategy list does not name")]
    UnlistedStrategy(Strategy),
}

impl Artifact {
    /// Parse and check an artifact's bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let file: ArtifactFile = serde_json::from_slice(bytes)
            .map_err(|error| ArtifactError::Format(error.to_string()))?;
        if file.schema_revision != ARTIFACT_SCHEMA_REVISION {
            return Err(ArtifactError::Schema {
                found: file.schema_revision,
            });
        }
        for (which, found, expected) in [
            ("input", file.input_revision, LEARNING_INPUT_REVISION),
            (
                "selector",
                file.selector_revision,
                LEARNED_SELECTOR_REVISION,
            ),
            // The `rules` pick every learned key embeds: an artifact
            // calibrated under another picker counted its prior under keys
            // that mean something else here.
            ("stage", file.stage_revision, STAGE_SELECTOR_REVISION),
            ("credit", file.credit_revision, LEARNING_CREDIT_REVISION),
        ] {
            if found != expected {
                return Err(ArtifactError::Revision {
                    which,
                    found,
                    expected,
                });
            }
        }
        if file.gate != GATE_NAME {
            return Err(ArtifactError::Gate(file.gate));
        }
        let strategies = StrategySet::new(file.strategies)
            .map_err(|error| ArtifactError::Strategies(error.to_string()))?;
        let mut entries = Vec::with_capacity(file.prior.len());
        for entry in file.prior {
            if !strategies.as_slice().contains(&entry.strategy) {
                return Err(ArtifactError::UnlistedStrategy(entry.strategy));
            }
            if entry.pos > entry.n {
                return Err(ArtifactError::PriorRange {
                    key: entry.key,
                    strategy: entry.strategy,
                    pos: entry.pos,
                    n: entry.n,
                });
            }
            if entries
                .iter()
                .any(|((key, strategy), _)| *key == entry.key && *strategy == entry.strategy)
            {
                return Err(ArtifactError::RepeatedPrior {
                    key: entry.key,
                    strategy: entry.strategy,
                });
            }
            entries.push((
                (entry.key, entry.strategy),
                Units {
                    pos: entry.pos,
                    n: entry.n,
                },
            ));
        }
        let epoch = epoch_of(bytes, &strategies);
        Ok(Self {
            strategies,
            prior: PriorUnits::new(entries),
            epoch,
            manifest_digest: file.manifest_digest,
            source_commit: file.source_commit,
        })
    }

    /// The strategy list the artifact was calibrated for, in its order.
    pub fn strategies(&self) -> &StrategySet {
        &self.strategies
    }

    pub fn prior(&self) -> &PriorUnits {
        &self.prior
    }

    /// The epoch this artifact starts.
    pub fn epoch(&self) -> EpochId {
        self.epoch
    }

    /// The digest of the session manifest the calibrator read.
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// The commit of the calibrator that wrote the artifact.
    pub fn source_commit(&self) -> &str {
        &self.source_commit
    }

    /// Consume the artifact into its strategies, prior and epoch.
    pub fn into_parts(self) -> (StrategySet, PriorUnits, EpochId) {
        (self.strategies, self.prior, self.epoch)
    }
}

/// The epoch id: SHA-256 over the SHA-256 of the artifact bytes, the ordered
/// strategy list and the four revisions an artifact names, truncated to 16
/// bytes (draft section 6).
///
/// `rules` is versioned by [`STAGE_SELECTOR_REVISION`], and a change to the
/// stage router's pick changes what a `rules` plan is, so that revision is
/// part of the id too.
pub fn epoch_of(bytes: &[u8], strategies: &StrategySet) -> EpochId {
    let artifact = hex::encode(Sha256::digest(bytes));
    let list = strategies
        .as_slice()
        .iter()
        .map(|strategy| strategy.label())
        .collect::<Vec<_>>()
        .join(",");
    let canonical = format!(
        "roundhouse-learner-epoch-v1\nartifact={artifact}\nstrategies={list}\n\
         input={LEARNING_INPUT_REVISION}\nselector={LEARNED_SELECTOR_REVISION}\n\
         stage={STAGE_SELECTOR_REVISION}\ncredit={LEARNING_CREDIT_REVISION}\n"
    );
    let digest = Sha256::digest(canonical.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    EpochId::new(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::learn::{Band, PriorBand};
    use crate::routing::stage::Tier;

    fn file(strategies: &str, prior: &str) -> String {
        format!(
            r#"{{"schema_revision":1,"input_revision":1,"selector_revision":1,"stage_revision":1,"credit_revision":1,
               "gate":"wilson-v1","strategies":{strategies},"prior":{prior},
               "manifest_digest":"00","source_commit":"abc"}}"#
        )
    }

    #[test]
    fn a_zero_prior_artifact_parses_and_names_its_epoch() {
        let bytes = file(r#"["rules","efficient","capable"]"#, "[]");
        let artifact = Artifact::parse(bytes.as_bytes()).unwrap();
        assert_eq!(
            artifact.strategies().as_slice(),
            &[Strategy::Rules, Strategy::Efficient, Strategy::Capable]
        );
        assert_eq!(
            artifact.epoch(),
            epoch_of(bytes.as_bytes(), artifact.strategies())
        );
    }

    #[test]
    fn a_prior_entry_is_read_by_key_and_strategy() {
        let key = LevelKey::L2 {
            rules_pick: Tier::Capable,
            newest: Band::Low,
            prior: PriorBand::NoHigh,
            tool_turn: false,
        };
        let prior = format!(
            r#"[{{"key":{},"strategy":"efficient","pos":900,"n":1000}}]"#,
            serde_json::to_string(&key).unwrap()
        );
        let artifact =
            Artifact::parse(file(r#"["rules","efficient"]"#, &prior).as_bytes()).unwrap();
        assert_eq!(
            artifact.prior().get(&key, Strategy::Efficient),
            Units { pos: 900, n: 1000 }
        );
        assert_eq!(
            artifact.prior().get(&key, Strategy::Rules),
            Units::default()
        );
    }

    #[test]
    fn one_byte_more_is_another_epoch() {
        let bytes = file(r#"["rules","capable"]"#, "[]");
        let spaced = format!("{bytes} ");
        let one = Artifact::parse(bytes.as_bytes()).unwrap().epoch();
        let two = Artifact::parse(spaced.as_bytes()).unwrap().epoch();
        assert_ne!(one, two);
    }

    #[test]
    fn the_strategy_order_is_part_of_the_epoch() {
        let bytes = b"same bytes";
        let one = StrategySet::new(vec![Strategy::Rules, Strategy::Capable]).unwrap();
        let two = StrategySet::new(vec![Strategy::Capable, Strategy::Rules]).unwrap();
        assert_ne!(epoch_of(bytes, &one), epoch_of(bytes, &two));
    }

    /// The epoch id is a key part of every learner-store counter, so a change
    /// to its encoding orphans every project's state on upgrade, and the
    /// offline calibrator must derive the same id. The expected value was
    /// computed outside the crate:
    ///
    /// ```text
    /// a = sha256(b"golden artifact bytes").hexdigest()
    /// c = "roundhouse-learner-epoch-v1\nartifact=" + a
    ///     + "\nstrategies=rules,efficient,capable\ninput=1\nselector=1\nstage=1\ncredit=1\n"
    /// sha256(c).hexdigest()[:32]
    /// ```
    #[test]
    fn the_epoch_matches_a_golden_digest() {
        let strategies = StrategySet::new(vec![
            Strategy::Rules,
            Strategy::Efficient,
            Strategy::Capable,
        ])
        .unwrap();
        assert_eq!(
            epoch_of(b"golden artifact bytes", &strategies).to_string(),
            "a0a82da1b79e03319379c3cd5223efad"
        );
    }

    #[test]
    fn every_content_rule_refuses_its_artifact() {
        let listed = r#"["rules","capable"]"#;
        let key = r#"{"level":"l0","rules_pick":"capable"}"#;
        for (which, field) in [
            ("input", "input_revision"),
            ("selector", "selector_revision"),
            ("stage", "stage_revision"),
        ] {
            let bytes =
                file(listed, "[]").replace(&format!(r#""{field}":1"#), &format!(r#""{field}":2"#));
            assert_eq!(
                Artifact::parse(bytes.as_bytes()),
                Err(ArtifactError::Revision {
                    which,
                    found: 2,
                    expected: 1
                })
            );
        }
        let gate = file(listed, "[]").replace("wilson-v1", "wilson-v2");
        assert_eq!(
            Artifact::parse(gate.as_bytes()),
            Err(ArtifactError::Gate("wilson-v2".into()))
        );
        let above = file(
            listed,
            &format!(r#"[{{"key":{key},"strategy":"rules","pos":3,"n":2}}]"#),
        );
        assert!(matches!(
            Artifact::parse(above.as_bytes()),
            Err(ArtifactError::PriorRange { pos: 3, n: 2, .. })
        ));
        let twice = file(
            listed,
            &format!(
                r#"[{{"key":{key},"strategy":"rules","pos":1,"n":2}},{{"key":{key},"strategy":"rules","pos":1,"n":2}}]"#
            ),
        );
        assert!(matches!(
            Artifact::parse(twice.as_bytes()),
            Err(ArtifactError::RepeatedPrior {
                strategy: Strategy::Rules,
                ..
            })
        ));
        // Control: the same entry once, at `pos == n`, is accepted.
        let once = file(
            listed,
            &format!(r#"[{{"key":{key},"strategy":"rules","pos":2,"n":2}}]"#),
        );
        assert!(Artifact::parse(once.as_bytes()).is_ok());
    }

    #[test]
    fn a_bad_artifact_is_refused_with_its_reason() {
        let bad_schema = file(r#"["rules","capable"]"#, "[]")
            .replace(r#""schema_revision":1"#, r#""schema_revision":2"#);
        assert_eq!(
            Artifact::parse(bad_schema.as_bytes()),
            Err(ArtifactError::Schema { found: 2 })
        );
        let no_rules = file(r#"["efficient","capable"]"#, "[]");
        assert!(matches!(
            Artifact::parse(no_rules.as_bytes()),
            Err(ArtifactError::Strategies(_))
        ));
        let credit = file(r#"["rules","capable"]"#, "[]")
            .replace(r#""credit_revision":1"#, r#""credit_revision":9"#);
        assert!(matches!(
            Artifact::parse(credit.as_bytes()),
            Err(ArtifactError::Revision {
                which: "credit",
                ..
            })
        ));
        let unknown = file(r#"["rules","capable"]"#, "[]").replace("}", r#","extra":1}"#);
        assert!(matches!(
            Artifact::parse(unknown.as_bytes()),
            Err(ArtifactError::Format(_))
        ));
        let unlisted = file(
            r#"["rules","capable"]"#,
            r#"[{"key":{"level":"l0","rules_pick":"capable"},"strategy":"efficient","pos":1,"n":2}]"#,
        );
        assert_eq!(
            Artifact::parse(unlisted.as_bytes()),
            Err(ArtifactError::UnlistedStrategy(Strategy::Efficient))
        );
    }
}
