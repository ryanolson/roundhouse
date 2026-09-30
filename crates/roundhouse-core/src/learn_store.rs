// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner store: the shared counters the online routing learner reads at
//! selection and adds learning entries to (draft section 11 of
//! `agent-docs/DRAFT-online-routing-learner.md`, milestone M6 of
//! `agent-docs/PLAN-online-routing-learner.md`).
//!
//! **Nothing calls it yet.** The engine composes a learner store in M8. Until
//! then no route reads these counters and no turn writes them.
//!
//! **Each entry applies exactly once, whoever sends it and however often.**
//! [`LearnerStore::apply`] compares every entry, not the batch, with the
//! `(project, session)` watermark: an entry at or below it is skipped by its
//! identity, and an entry above it applies only if its `prev_seq` equals the
//! watermark as it stands after the entries before it. A resend after a lost
//! acknowledgement therefore skips what landed and applies what did not, and a
//! batch that starts after a missing entry is refused with
//! [`LearnerError::ChainGap`] rather than moving the watermark past the entry
//! it never saw. An entry whose predecessor is below the watermark is refused
//! with [`LearnerError::ChainDiverged`]: its chain lacks the store's entry, and
//! no backfill can supply it. A batch-level comparison fails both ways: it drops the new
//! entries of a resend, or it skips the missing one for good (draft section
//! 15). The rule does not depend on the session lease, because every node
//! folds the same entries from the same log.
//!
//! **All or nothing.** `apply` checks the whole batch before it writes
//! anything: the chain, every new counter value against its range, and (in a
//! backend that has them) key types. A refused batch leaves the store as it
//! was. The Redis backend of M7 cannot undo a write once its script has made
//! one, so the check has to come first there, and the memory backend keeps the
//! same order so that one contract judges both.
//!
//! **Integers within `2^53 - 1`.** The Redis backend adds in Lua, whose numbers
//! are doubles and hold integers exactly only up to [`MAX_EXACT`]. Sequences
//! and counters share that limit so both backends refuse the same batches.

#[cfg(any(test, feature = "test-support"))]
pub mod contract;
pub mod memory;

use async_trait::async_trait;

use crate::control::ProjectId;
use crate::ids::SessionId;
use crate::routing::Target;
use crate::routing::learn::{EpochId, LearnedInput, LevelKey, ReadView, Strategy, StrategySet};
use crate::session::{Deltas, LearningEntry};

pub use memory::MemoryLearnerStore;

/// The largest integer a Lua number holds exactly, `2^53 - 1`.
///
/// The bound on every sequence and every nonnegative counter, and on the
/// magnitude of every signed sum.
pub const MAX_EXACT: u64 = (1 << 53) - 1;

/// The shared counters of the online routing learner.
///
/// Every method is scoped by the project from the session principal. No
/// method reads one project and writes another.
#[async_trait]
pub trait LearnerStore: Send + Sync + 'static {
    /// The counters of one turn, one epoch: its three quality keys and the
    /// operations key. Makes no write.
    ///
    /// The view holds every requested key, strategy and target in request
    /// order, with zeros where the store holds nothing. The view is recorded
    /// on every learned `Routed`, so two backends that shaped it differently
    /// would write different logs for the same state.
    ///
    /// Fails only with [`LearnerError::Unavailable`] (M7 adds the wrong-type
    /// refusal, which a read can meet too). The other variants judge a batch,
    /// and a read carries none.
    async fn read(&self, request: &ReadRequest) -> Result<ReadView, LearnerError>;

    /// Apply the entries above the `(project, session)` watermark, all or
    /// nothing.
    ///
    /// On success returns how many entries applied and the watermark after
    /// them, which is the store's watermark even when nothing applied: a
    /// batch wholly at or below it returns the larger value, and that is what
    /// the caller may confirm to the source index.
    ///
    /// The two chain refusals ask for different recoveries.
    /// [`LearnerError::ChainGap`] means "backfill from this watermark": an
    /// entry names a predecessor above it, which the store has not seen.
    /// [`LearnerError::ChainDiverged`] means "stop this session's delivery and
    /// report; do not backfill": an entry names a predecessor below it, so the
    /// store holds an entry the sender's chain does not have, and a backfill
    /// from the watermark would resend the same entry forever.
    async fn apply(&self, batch: &LearningBatch<'_>) -> Result<Applied, LearnerError>;

    /// The `(project, session)` watermark, 0 for a session with no entry
    /// applied. Makes no write.
    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<u64, LearnerError>;
}

/// What one turn reads: the keys of its learned input under one epoch.
///
/// **Built from a [`LearnedInput`], so the keys are always the three levels of
/// one turn.** A request that could name any three keys could read a level
/// the gate would then attribute to the wrong input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRequest {
    project: ProjectId,
    epoch: EpochId,
    keys: [LevelKey; 3],
    strategies: Vec<Strategy>,
    targets: Vec<String>,
}

impl ReadRequest {
    /// The read for one turn.
    ///
    /// `targets` are the recipe's targets. They are named by policy identity,
    /// the way [`ReadView::target`] finds them, and a target listed twice is
    /// read once, so every worker of one local model reads one row.
    pub fn new<'a>(
        project: ProjectId,
        epoch: EpochId,
        input: &LearnedInput,
        strategies: &StrategySet,
        targets: impl IntoIterator<Item = &'a Target>,
    ) -> Self {
        let mut identities: Vec<String> = Vec::new();
        for target in targets {
            let identity = target.policy_identity();
            if !identities.contains(&identity) {
                identities.push(identity);
            }
        }
        Self {
            project,
            epoch,
            keys: input.keys(),
            strategies: strategies.as_slice().to_vec(),
            targets: identities,
        }
    }

    pub fn project(&self) -> &ProjectId {
        &self.project
    }

    pub fn epoch(&self) -> EpochId {
        self.epoch
    }

    /// Most specific first.
    pub fn keys(&self) -> &[LevelKey; 3] {
        &self.keys
    }

    /// In configured order.
    pub fn strategies(&self) -> &[Strategy] {
        &self.strategies
    }

    /// Policy identities, first occurrence first.
    pub fn targets(&self) -> &[String] {
        &self.targets
    }
}

/// The entries of one session for one project, in ascending `seq`.
///
/// **Borrows its entries.** The engine sends the fold's page on every turn
/// tail that has one; owning the entries would clone that page each time only
/// for the store to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearningBatch<'a> {
    pub project: &'a ProjectId,
    pub session: &'a SessionId,
    pub entries: &'a [LearningEntry],
}

impl LearningBatch<'_> {
    /// The checks that do not depend on stored state, which every backend runs
    /// before it reads anything.
    ///
    /// **[`LearnerError::Malformed`] is about the input alone;
    /// [`LearnerError::CounterRange`] is about the input against the stored
    /// counters.** A batch that fails here fails against every state, so the
    /// split tells an operator whether the build or the data is at fault.
    ///
    /// Refused: a sequence above [`MAX_EXACT`], sequences that do not ascend
    /// strictly from 1, a `prev_seq` not below its `seq`, a nonnegative delta
    /// above [`MAX_EXACT`], and a signed delta beyond `±(2^53 - 1)`. A
    /// negative delta for a nonnegative counter reaches these `u64` fields
    /// only by a wrapping cast, which puts it far above [`MAX_EXACT`], so it is
    /// refused by the same check. Each rule is held by a contract case that
    /// the chain rule alone would answer differently.
    pub fn check(&self) -> Result<(), LearnerError> {
        let mut last = 0;
        for entry in self.entries {
            let malformed = |reason: String| LearnerError::Malformed {
                reason: format!("entry {}: {reason}", entry.seq),
            };
            // `prev_seq` needs no bound of its own: it must be below `seq`,
            // which is within the bound, and a second clause here could never
            // be the one that refuses.
            if entry.seq > MAX_EXACT {
                return Err(malformed("a sequence above 2^53 - 1".to_owned()));
            }
            if entry.seq <= last {
                return Err(malformed(format!(
                    "sequences must ascend strictly, after {last}"
                )));
            }
            if entry.prev_seq >= entry.seq {
                return Err(malformed(format!(
                    "prev_seq {} is not below seq",
                    entry.prev_seq
                )));
            }
            if let Some(deltas) = &entry.deltas {
                check_deltas(deltas)
                    .map_err(|field| malformed(format!("{field} is out of range")))?;
            }
            last = entry.seq;
        }
        Ok(())
    }
}

/// Every delta a Lua number carries exactly. Returns the offending field.
fn check_deltas(deltas: &Deltas) -> Result<(), &'static str> {
    let unsigned = |value: u64, field: &'static str| {
        if value <= MAX_EXACT {
            Ok(())
        } else {
            Err(field)
        }
    };
    let signed = |value: i64, field: &'static str| {
        if value.unsigned_abs() <= MAX_EXACT {
            Ok(())
        } else {
            Err(field)
        }
    };
    for quality in &deltas.quality {
        unsigned(quality.units.pos, "pos")?;
        unsigned(quality.units.n, "n")?;
    }
    for target in &deltas.targets {
        signed(target.latency.sum_ms, "lat_sum")?;
        unsigned(target.latency.n, "lat_n")?;
        unsigned(target.failover, "failover")?;
        unsigned(target.cache.predicted_permille, "cache_pred")?;
        unsigned(target.cache.observed_permille, "cache_obs")?;
        unsigned(target.cache.n, "cache_n")?;
    }
    signed(deltas.overhead.sum_ms, "pre_sum")?;
    unsigned(deltas.overhead.n, "pre_n")?;
    for jev in &deltas.jev {
        unsigned(jev.counts.capable, "jev_capable")?;
        unsigned(jev.counts.efficient, "jev_efficient")?;
    }
    Ok(())
}

/// A successful apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// Entries applied by this call. Skipped entries are not counted.
    pub applied: usize,
    /// The store watermark after the call.
    pub watermark: u64,
}

/// Why a learner-store call did not succeed.
///
/// `ChainGap` asks for a backfill. `ChainDiverged` does not go away on a
/// retry or a backfill: the engine stops delivery for that one session and
/// reports it, and the project's other sessions carry on. `CounterRange` and
/// `Malformed` do not go away on a retry either: the engine stops updates for
/// the project epoch and a new epoch is the recovery (draft section 11.3).
/// `Unavailable` leaves the result unknown, and a resend is safe under the
/// identity rule.
///
/// A wrong-type refusal joins with the Redis backend (M7), the only backend
/// that can hold a key of the wrong type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LearnerError {
    /// The batch skips an entry. Backfill from `store_watermark`, the
    /// watermark the store holds, not one the refused batch would have moved
    /// it to.
    #[error("the batch skips an entry: the store watermark is {store_watermark}")]
    ChainGap { store_watermark: u64 },
    /// The sender's chain has no entry at the store watermark: an entry above
    /// it names a predecessor below it. Stop this session's delivery and
    /// report it; a backfill from `store_watermark` would reproduce the same
    /// entry. Nothing was written.
    #[error("the batch's chain diverges from the store at watermark {store_watermark}")]
    ChainDiverged { store_watermark: u64 },
    /// A counter would leave its range. Nothing was written.
    #[error("counter `{counter}` would leave its range")]
    CounterRange { counter: String },
    /// The batch is invalid whatever the store holds. Nothing was written.
    #[error("malformed learning batch: {reason}")]
    Malformed { reason: String },
    /// The store did not answer. Whether the call took effect is unknown.
    #[error("learner store unavailable: {0}")]
    Unavailable(String),
}
