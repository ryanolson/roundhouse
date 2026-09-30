// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The [`LearnerStore`] contract as executable assertions.
//!
//! Every guarantee the trait documents lives here as a test any backend must
//! pass unchanged, as [`store::contract`](crate::store::contract) does for the
//! session store. The memory backend runs it, and the Redis backend runs the
//! same list in `roundhouse-store-redis`'s `tests/learn_contract.rs`, which is
//! what makes "a Lua script and a `HashMap` keep the same counters" a checked
//! property. The cases carry the store's model check
//! into Rust: the lost acknowledgement, the chain gap, the
//! overflow with no change, and store loss repaired by a backfill. The
//! diverged chain, which no backfill repairs, is a case of its own.
//!
//! Every test mints a fresh [`ProjectId`], and every key and watermark is
//! scoped by project, so one shared backend (one real Redis) hosts the whole
//! suite without one test reading another's counters.
//!
//! The [`learner_store_contract_suite!`](crate::learner_store_contract_suite)
//! macro is the single list. The cases of the memory store's test-support
//! instrumentation are not in it, because only that store has it: the fault
//! between the check and the write, which keys a read visits, and when each
//! instrument is armed. They are in [`memory`](super::memory)'s own tests.

pub mod isolation;

use async_trait::async_trait;

use crate::control::ProjectId;
use crate::ids::SessionId;
use crate::learn_store::{
    Applied, LearnerError, LearnerStore, LearningBatch, MAX_EXACT, ReadRequest,
};
use crate::routing::learn::{
    CacheReuse, EpochId, JevCounts, LEARNING_CREDIT_REVISION, LatencySum, LearnedInput, LevelKey,
    ReadView, Strategy, StrategyCounts, StrategySet, Units,
};
use crate::routing::{Target, Tier};
use crate::session::{Deltas, JevDelta, LearningEntry, QualityDelta, TargetDelta};
use crate::validate::REVIEW_RULE_REVISION;

/// Store-side levers the suite needs and the trait must not offer.
///
/// `snapshot` is every stored value of one project, watermarks and `seen`
/// sets included, in whatever form the backend can compare: the suite checks
/// "changed nothing" and "made no write" by equality, which no read through
/// the trait could prove. `restore` puts a snapshot back, which is how the
/// suite stages the loss of recent writes. The memory
/// backend clones its project state; a Redis backend dumps and restores the
/// keys under the project's prefix. Test-only by construction, like
/// [`LeaseControl`](crate::store::contract::LeaseControl).
///
/// **A snapshot tells an absent project from an empty one.** A read, a
/// watermark query, or an apply that applied nothing must leave no record of
/// a project it found empty, and only a snapshot that sees existence can hold
/// that. The memory backend returns `None` for a project it has no entry for,
/// and restoring `None` removes the entry. Redis cannot hold an empty key, so
/// its snapshot of an absent project is the empty key set, and a restore of
/// that set deletes every key under the prefix.
#[async_trait]
pub trait LearnerStoreControl: LearnerStore {
    type Snapshot: std::fmt::Debug + Clone + PartialEq + Send;

    async fn snapshot(&self, project: &ProjectId) -> Self::Snapshot;

    async fn restore(&self, project: &ProjectId, snapshot: Self::Snapshot);
}

/// A project no other test uses.
pub fn fresh_project() -> ProjectId {
    ProjectId::new(format!("proj_{}", uuid::Uuid::new_v4().simple()))
}

pub fn epoch(byte: u8) -> EpochId {
    EpochId::new([byte; 16])
}

/// A turn's input with no classification: `rules_pick` and the tool flag are
/// the only fields that vary its keys here.
pub fn input(rules_pick: Tier, tool_turn: bool) -> LearnedInput {
    LearnedInput::encode(rules_pick, tool_turn, None, &[])
}

pub fn all_strategies() -> StrategySet {
    StrategySet::new(vec![
        Strategy::Rules,
        Strategy::Efficient,
        Strategy::Capable,
    ])
    .expect("the three strategies are a valid set")
}

pub fn frontier(model: &str) -> Target {
    Target::Frontier {
        provider: "anthropic".to_owned(),
        model: model.to_owned(),
    }
}

pub fn entry(seq: u64, prev_seq: u64, deltas: Option<Deltas>) -> LearningEntry {
    LearningEntry {
        seq,
        prev_seq,
        credit_revision: LEARNING_CREDIT_REVISION,
        review_rule_revision: REVIEW_RULE_REVISION,
        deltas,
    }
}

/// Deltas that credit `units` to `strategy` at `key`, and nothing else.
pub fn credit(epoch: EpochId, key: LevelKey, strategy: Strategy, pos: u64, n: u64) -> Deltas {
    Deltas {
        epoch,
        quality: vec![QualityDelta {
            key,
            strategy,
            units: Units { pos, n },
        }],
        targets: Vec::new(),
        overhead: LatencySum::default(),
        jev: Vec::new(),
    }
}

/// Deltas that add one latency residual to `target`.
pub fn residual(epoch: EpochId, target: &Target, sum_ms: i64) -> Deltas {
    Deltas {
        epoch,
        quality: Vec::new(),
        targets: vec![TargetDelta {
            target: target.policy_identity(),
            latency: LatencySum { sum_ms, n: 1 },
            failover: 0,
            cache: CacheReuse::default(),
        }],
        overhead: LatencySum::default(),
        jev: Vec::new(),
    }
}

/// Deltas that add one Jev answer on every key of `input` and one overhead
/// sample.
pub fn jev_and_overhead(
    epoch: EpochId,
    input: &LearnedInput,
    counts: JevCounts,
    overhead_ms: i64,
) -> Deltas {
    Deltas {
        epoch,
        quality: Vec::new(),
        targets: Vec::new(),
        overhead: LatencySum {
            sum_ms: overhead_ms,
            n: 1,
        },
        jev: input
            .keys()
            .into_iter()
            .map(|key| JevDelta { key, counts })
            .collect(),
    }
}

pub fn batch<'a>(
    project: &'a ProjectId,
    session: &'a SessionId,
    entries: &'a [LearningEntry],
) -> LearningBatch<'a> {
    LearningBatch {
        project,
        session,
        entries,
    }
}

/// The L0 key of `input`: the one level every test that needs a single key
/// credits.
pub fn l0(input: &LearnedInput) -> LevelKey {
    input.keys()[2]
}

pub async fn read_view<S: LearnerStore>(
    store: &S,
    project: &ProjectId,
    epoch: EpochId,
    input: &LearnedInput,
) -> ReadView {
    let request = ReadRequest::new(
        project.clone(),
        epoch,
        input,
        &all_strategies(),
        [&frontier("large")],
    );
    store.read(&request).await.expect("the read succeeds")
}

/// The counts of `strategy` at `key`, as a read of `input` returns them.
pub async fn counts<S: LearnerStore>(
    store: &S,
    project: &ProjectId,
    epoch: EpochId,
    input: &LearnedInput,
    key: LevelKey,
    strategy: Strategy,
) -> StrategyCounts {
    let view = read_view(store, project, epoch, input).await;
    let level = view
        .levels
        .iter()
        .find(|level| level.key == key)
        .unwrap_or_else(|| panic!("the read returns every key of the turn, not {key}"));
    *level
        .strategies
        .iter()
        .find(|counts| counts.strategy == strategy)
        .unwrap_or_else(|| panic!("the read returns every requested strategy, not {strategy}"))
}

async fn apply<S: LearnerStore>(store: &S, batch: &LearningBatch<'_>) -> Applied {
    store
        .apply(batch)
        .await
        .unwrap_or_else(|error| panic!("the batch applies: {error}"))
}

/// A chain of entries 10, 20, ... each crediting `rules` at the L0 key of
/// `input` with `n = 1 << index`, so any entry applied twice or lost shows in
/// the sum.
pub fn chain(epoch: EpochId, input: &LearnedInput, len: usize) -> Vec<LearningEntry> {
    (0..len)
        .map(|index| {
            let seq = 10 * (index as u64 + 1);
            entry(
                seq,
                seq - 10,
                Some(credit(epoch, l0(input), Strategy::Rules, 0, 1 << index)),
            )
        })
        .collect()
}

pub async fn apply_then_read_returns_the_integer_sums<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let [l2, l1, l0] = turn.keys();
    let entries = vec![
        entry(10, 0, Some(credit(e, l2, Strategy::Rules, 700, 1000))),
        entry(20, 10, Some(credit(e, l1, Strategy::Rules, 300, 500))),
        entry(30, 20, Some(credit(e, l1, Strategy::Rules, 200, 500))),
        entry(40, 30, Some(residual(e, &frontier("large"), -35))),
        entry(50, 40, Some(residual(e, &frontier("large"), 12))),
        entry(60, 50, None),
    ];
    let applied = apply(store, &batch(&project, &session, &entries)).await;
    assert_eq!(
        applied,
        Applied {
            applied: 6,
            watermark: 60
        },
        "every entry above the watermark applies, an entry with no deltas included"
    );

    let view = read_view(store, &project, e, &turn).await;
    let rules = |key: LevelKey| {
        view.levels
            .iter()
            .find(|level| level.key == key)
            .and_then(|level| {
                level
                    .strategies
                    .iter()
                    .find(|c| c.strategy == Strategy::Rules)
            })
            .copied()
            .expect("every key and strategy is read")
    };
    assert_eq!((rules(l2).pos_units, rules(l2).n_units), (700, 1000));
    assert_eq!((rules(l1).pos_units, rules(l1).n_units), (500, 1000));
    assert_eq!((rules(l0).pos_units, rules(l0).n_units), (0, 0));
    let ops = view.target(&frontier("large")).expect("the target is read");
    assert_eq!(
        ops.latency,
        LatencySum { sum_ms: -23, n: 2 },
        "residuals are a signed sum and its count"
    );
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 60);
}

pub async fn a_repeated_batch_applies_zero_entries<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let entries = chain(e, &turn, 3);
    let sent = batch(&project, &session, &entries);
    apply(store, &sent).await;
    let before = store.snapshot(&project).await;

    let again = apply(store, &sent).await;
    assert_eq!(
        again,
        Applied {
            applied: 0,
            watermark: 30
        }
    );
    assert_eq!(
        store.snapshot(&project).await,
        before,
        "a resend of applied entries changes nothing"
    );
}

/// The parent scenario: entry 10 applies but its
/// acknowledgement is lost, then the sender resends 10 with the new 20. The
/// total is 2, not 3.
pub async fn a_lost_ack_then_a_new_entry_credits_each_entry_once<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    let first = entry(10, 0, Some(credit(e, key, Strategy::Rules, 1, 1)));
    let second = entry(20, 10, Some(credit(e, key, Strategy::Rules, 1, 1)));

    apply(
        store,
        &batch(&project, &session, std::slice::from_ref(&first)),
    )
    .await;
    let resent = apply(store, &batch(&project, &session, &[first, second])).await;

    assert_eq!(
        resent,
        Applied {
            applied: 1,
            watermark: 20
        },
        "the resend skips entry 10 by its identity and applies entry 20"
    );
    let counts = counts(store, &project, e, &turn, key, Strategy::Rules).await;
    assert_eq!((counts.pos_units, counts.n_units), (2, 2));
}

pub async fn a_late_original_after_a_larger_retry_applies_zero_entries<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let entries = chain(e, &turn, 3);
    let original = batch(&project, &session, &entries[..2]);
    let retry = batch(&project, &session, &entries);

    apply(store, &retry).await;
    let before = store.snapshot(&project).await;
    let late = apply(store, &original).await;

    assert_eq!(
        late,
        Applied {
            applied: 0,
            watermark: 30
        },
        "the late original returns the store watermark, not its own last seq: \
         that value is what the caller confirms to the source index"
    );
    assert_eq!(store.snapshot(&project).await, before);
}

/// A request that was in flight while newer entries landed, and that also
/// carries one entry nobody applied yet.
pub async fn a_stale_request_with_a_newer_entry_applies_only_that_entry<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let entries = chain(e, &turn, 4);
    apply(store, &batch(&project, &session, &entries[..3])).await;

    let stale = apply(store, &batch(&project, &session, &entries[1..])).await;

    assert_eq!(
        stale,
        Applied {
            applied: 1,
            watermark: 40
        }
    );
    let counts = counts(store, &project, e, &turn, l0(&turn), Strategy::Rules).await;
    assert_eq!(
        counts.n_units, 0b1111,
        "each of the four entries counted once"
    );
}

/// Entry 10 would stage, then entry 30 names 20 as its predecessor. The whole
/// batch is refused, entry 10 included, and the gap reports the watermark the
/// store holds, not the 10 the batch staged.
pub async fn a_batch_that_skips_an_entry_returns_chain_gap_and_changes_nothing<
    S: LearnerStoreControl,
>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    let before = store.snapshot(&project).await;

    let gapped_entries = [
        entry(10, 0, Some(credit(e, key, Strategy::Rules, 1, 1))),
        entry(30, 20, Some(credit(e, key, Strategy::Rules, 1, 1))),
    ];
    let gapped = batch(&project, &session, &gapped_entries);
    assert_eq!(
        store.apply(&gapped).await,
        Err(LearnerError::ChainGap { store_watermark: 0 })
    );
    assert_eq!(
        store.snapshot(&project).await,
        before,
        "a refused batch writes nothing, not even the entry before the gap"
    );
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 0);
}

/// An entry above the watermark whose predecessor is below it is not a gap.
/// The store holds an entry at its watermark that the sender's chain does not
/// have, so a backfill from the watermark would send the same entry forever.
/// It is refused as `ChainDiverged`, which the engine reports and never
/// backfills. The rule compares with the watermark as the batch's earlier
/// entries moved it, so a batch that diverges from its own staged entry is
/// refused the same way, and still reports the watermark the store holds. A
/// batch that begins with an entry the store already holds, which skips by
/// identity, is still refused when the next entry skips over the watermark: a
/// backend that checked only the first entry against the watermark, and each
/// later entry against the entry before it, would accept that diverged chain.
pub async fn a_diverged_chain_is_refused_and_is_not_a_gap<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    apply(store, &batch(&project, &session, &chain(e, &turn, 2))).await;
    let before = store.snapshot(&project).await;
    let next = |seq, prev_seq| entry(seq, prev_seq, Some(credit(e, key, Strategy::Rules, 1, 1)));
    let refused = |entries: Vec<LearningEntry>| {
        let (project, session) = (project.clone(), session.clone());
        async move { store.apply(&batch(&project, &session, &entries)).await }
    };

    assert_eq!(
        refused(vec![next(30, 10)]).await,
        Err(LearnerError::ChainDiverged {
            store_watermark: 20
        }),
        "entry 30 skips over the store's entry 20"
    );
    assert_eq!(
        refused(vec![next(30, 20), next(40, 25)]).await,
        Err(LearnerError::ChainDiverged {
            store_watermark: 20
        }),
        "entry 40 skips over the batch's own entry 30"
    );
    assert_eq!(
        refused(vec![next(10, 0), next(30, 10)]).await,
        Err(LearnerError::ChainDiverged {
            store_watermark: 20
        }),
        "entry 10 skips by identity, and entry 30 still skips over the store's entry 20"
    );
    assert_eq!(
        refused(vec![next(30, 25)]).await,
        Err(LearnerError::ChainGap {
            store_watermark: 20
        }),
        "control: a predecessor above the watermark is a gap a backfill fills"
    );
    assert_eq!(store.snapshot(&project).await, before);
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 20);
}

/// Store loss: the store applied 10 and 20, then 30, then lost the
/// write of 30. A page above the log's hint of 30 gaps at 20, and the backfill
/// from 20 restores what a rebuild from the log gives.
///
/// Entries 10 and 20 credit `rules` and the lost 30 and the later 40 credit
/// `capable`, so the loss takes a `seen` member with it. A restore that kept
/// the live `seen` set would let the backfill find `capable` already seen and
/// leave its `sessions` at 0, which no rebuild from the log could produce.
pub async fn after_store_loss_a_gap_backfill_restores_the_rebuilt_totals<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    let entries = [
        entry(10, 0, Some(credit(e, key, Strategy::Rules, 0, 1))),
        entry(20, 10, Some(credit(e, key, Strategy::Rules, 0, 2))),
        entry(30, 20, Some(credit(e, key, Strategy::Capable, 0, 4))),
        entry(40, 30, Some(credit(e, key, Strategy::Capable, 0, 8))),
    ];
    apply(store, &batch(&project, &session, &entries[..2])).await;
    let after_twenty = store.snapshot(&project).await;
    apply(store, &batch(&project, &session, &entries[2..3])).await;
    store.restore(&project, after_twenty).await;
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 20);

    let page = batch(&project, &session, &entries[3..]);
    assert_eq!(
        store.apply(&page).await,
        Err(LearnerError::ChainGap {
            store_watermark: 20
        })
    );
    let backfill = apply(store, &batch(&project, &session, &entries[2..])).await;

    assert_eq!(
        backfill,
        Applied {
            applied: 2,
            watermark: 40
        }
    );
    let totals = |counts: StrategyCounts| (counts.n_units, counts.sessions);
    assert_eq!(
        totals(counts(store, &project, e, &turn, key, Strategy::Rules).await),
        (0b0011, 1),
        "the entries before the loss"
    );
    assert_eq!(
        totals(counts(store, &project, e, &turn, key, Strategy::Capable).await),
        (0b1100, 1),
        "the lost entry and the next, rebuilt with their session"
    );
}

/// A batch whose second entry would carry a counter past `2^53 - 1` is
/// refused whole: the first entry, which was in range on its own, is not
/// written either.
pub async fn an_out_of_range_result_is_refused_and_changes_nothing<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    apply(
        store,
        &batch(
            &project,
            &session,
            &[entry(
                10,
                0,
                Some(credit(e, key, Strategy::Rules, 0, MAX_EXACT - 1)),
            )],
        ),
    )
    .await;
    let before = store.snapshot(&project).await;

    let over_entries = [
        entry(20, 10, Some(credit(e, key, Strategy::Capable, 1, 1))),
        entry(30, 20, Some(credit(e, key, Strategy::Rules, 0, 2))),
    ];
    let over = batch(&project, &session, &over_entries);
    assert!(
        matches!(
            store.apply(&over).await,
            Err(LearnerError::CounterRange { .. })
        ),
        "n would reach 2^53"
    );
    assert_eq!(store.snapshot(&project).await, before);
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 10);

    // One below the limit is still a counter.
    let at_limit_entries = [entry(20, 10, Some(credit(e, key, Strategy::Rules, 0, 1)))];
    let at_limit = batch(&project, &session, &at_limit_entries);
    assert_eq!(apply(store, &at_limit).await.applied, 1);
    let counts = counts(store, &project, e, &turn, key, Strategy::Rules).await;
    assert_eq!(counts.n_units, MAX_EXACT);
}

/// A negative delta reaches a `u64` counter field only through a wrapping
/// cast, so that is the form this test sends: `-1i64 as u64`. It is refused as
/// malformed input, whatever the store holds, rather than added or reported as
/// a range failure of the stored counter.
pub async fn a_negative_delta_for_a_nonnegative_counter_is_refused<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let before = store.snapshot(&project).await;

    let negative_entries = [entry(
        10,
        0,
        Some(credit(e, l0(&turn), Strategy::Rules, 0, -1i64 as u64)),
    )];
    let negative = batch(&project, &session, &negative_entries);
    assert!(
        matches!(
            store.apply(&negative).await,
            Err(LearnerError::Malformed { .. })
        ),
        "a wrapped negative delta is malformed input"
    );
    assert_eq!(store.snapshot(&project).await, before);
}

pub async fn a_sequence_above_the_exact_lua_range_is_refused<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let before = store.snapshot(&project).await;

    let beyond_entries = [entry(
        MAX_EXACT + 1,
        0,
        Some(credit(e, l0(&turn), Strategy::Rules, 1, 1)),
    )];
    let beyond = batch(&project, &session, &beyond_entries);
    assert!(matches!(
        store.apply(&beyond).await,
        Err(LearnerError::Malformed { .. })
    ));
    assert_eq!(store.snapshot(&project).await, before);

    let at_limit_entries = [entry(
        MAX_EXACT,
        0,
        Some(credit(e, l0(&turn), Strategy::Rules, 1, 1)),
    )];
    let at_limit = batch(&project, &session, &at_limit_entries);
    assert_eq!(
        apply(store, &at_limit).await,
        Applied {
            applied: 1,
            watermark: MAX_EXACT
        }
    );
}

/// Every input rule of [`LearningBatch::check`] refuses its batch as
/// `Malformed`. Each case is one that the chain rule alone answers
/// differently, so the case holds its rule: without the ascending rule the
/// out-of-order entry is skipped and the batch succeeds, without the
/// predecessor rule a `prev_seq` at or above `seq` is a gap that a backfill
/// can never close, and without the delta bound `i64::MIN` is reported as a
/// range failure of the stored counter rather than as bad input.
pub async fn every_input_rule_refuses_its_batch_as_malformed<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let rules = |seq, prev_seq| {
        entry(
            seq,
            prev_seq,
            Some(credit(e, l0(&turn), Strategy::Rules, 1, 1)),
        )
    };
    let cases = [
        (
            "sequences that do not ascend",
            vec![rules(10, 0), rules(20, 10), rules(15, 10)],
        ),
        ("a prev_seq equal to its seq", vec![rules(10, 10)]),
        // A second case of the rule above: `seq` is already bounded, so a
        // `prev_seq` past the bound is also at or above its `seq`.
        ("a prev_seq above 2^53 - 1", vec![rules(10, MAX_EXACT + 1)]),
        (
            "a signed delta beyond -(2^53 - 1)",
            vec![entry(
                10,
                0,
                Some(residual(e, &frontier("large"), i64::MIN)),
            )],
        ),
    ];
    let before = store.snapshot(&project).await;
    // Every case runs before the verdict, so a failure names each rule a
    // backend misses rather than only the first.
    let mut missed = Vec::new();
    for (rule, entries) in cases {
        let refused = store.apply(&batch(&project, &session, &entries)).await;
        if !matches!(refused, Err(LearnerError::Malformed { .. })) {
            missed.push(format!("{rule}: {refused:?}"));
        }
        if store.snapshot(&project).await != before {
            missed.push(format!("{rule}: the store changed"));
            store.restore(&project, before.clone()).await;
        }
    }
    assert!(
        missed.is_empty(),
        "rules not refused as malformed: {missed:#?}"
    );
}

/// The signed sums are checked after every entry, not only at the end of the
/// batch. A Lua number beyond `2^53` is no longer exact, so a sum that passes
/// through it and comes back could land on a different integer than it left.
pub async fn a_signed_sum_is_range_checked_after_every_entry<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let target = frontier("large");
    let near = MAX_EXACT as i64 - 5;
    apply(
        store,
        &batch(
            &project,
            &session,
            &[entry(10, 0, Some(residual(e, &target, near)))],
        ),
    )
    .await;
    let before = store.snapshot(&project).await;

    let through_entries = [
        entry(20, 10, Some(residual(e, &target, 10))),
        entry(30, 20, Some(residual(e, &target, -10))),
    ];
    let through = batch(&project, &session, &through_entries);
    assert!(
        matches!(
            store.apply(&through).await,
            Err(LearnerError::CounterRange { .. })
        ),
        "the sum passes 2^53 - 1 after entry 20, though it ends in range"
    );
    assert_eq!(store.snapshot(&project).await, before);

    let negative_entries = [
        entry(20, 10, Some(residual(e, &target, -near))),
        entry(30, 20, Some(residual(e, &target, -near))),
    ];
    let negative = batch(&project, &session, &negative_entries);
    assert_eq!(
        apply(store, &negative).await.applied,
        2,
        "a signed sum may go negative, down to -(2^53 - 1)"
    );
}

/// Jev counts and overhead sums are deltas like any other: a resend after a
/// lost acknowledgement adds them once.
pub async fn jev_counts_and_overhead_sums_apply_under_the_same_identity_rule<
    S: LearnerStoreControl,
>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Efficient, true);
    let capable = JevCounts {
        capable: 1,
        efficient: 0,
    };
    let efficient = JevCounts {
        capable: 0,
        efficient: 1,
    };
    let first = entry(10, 0, Some(jev_and_overhead(e, &turn, capable, 40)));
    let second = entry(20, 10, Some(jev_and_overhead(e, &turn, efficient, 60)));

    apply(
        store,
        &batch(&project, &session, std::slice::from_ref(&first)),
    )
    .await;
    apply(store, &batch(&project, &session, &[first.clone(), second])).await;
    apply(store, &batch(&project, &session, &[first])).await;

    let view = read_view(store, &project, e, &turn).await;
    for level in &view.levels {
        assert_eq!(
            level.jev,
            JevCounts {
                capable: 1,
                efficient: 1
            },
            "each answer counted once at {}",
            level.key
        );
    }
    assert_eq!(view.overhead, LatencySum { sum_ms: 100, n: 2 });
}

/// Instantiate the whole learner store conformance suite against one backend.
///
/// The single list of contract tests, in the style of
/// [`store_contract_suite!`](crate::store_contract_suite): one `#[tokio::test]`
/// per name, a fresh store per test from `$make`, and an optional `ignore =
/// "…"` gate the whole suite shares. The backend must implement
/// [`LearnerStoreControl`].
///
/// ```ignore
/// roundhouse_core::learner_store_contract_suite!(MemoryLearnerStore::new());
///
/// roundhouse_core::learner_store_contract_suite!(
///     ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
///     connect_from_env().await
/// );
/// ```
///
/// Only usable where the `contract` module is compiled: this crate's own
/// tests, or a dependent with the `test-support` feature on its
/// dev-dependency.
#[macro_export]
macro_rules! learner_store_contract_suite {
    (ignore = $reason:literal, $make:expr $(,)?) => {
        $crate::learner_store_contract_suite!(@list (#[ignore = $reason]) $make);
    };
    ($make:expr $(,)?) => {
        $crate::learner_store_contract_suite!(@list () $make);
    };
    // The single list. Both public arms land here, so gated and ungated
    // backends cannot drift apart in coverage.
    (@list $attrs:tt $make:expr) => {
        $crate::__contract_suite!(store, $crate::learn_store::contract, $attrs, $make;
            apply_then_read_returns_the_integer_sums,
            a_repeated_batch_applies_zero_entries,
            a_lost_ack_then_a_new_entry_credits_each_entry_once,
            a_late_original_after_a_larger_retry_applies_zero_entries,
            a_stale_request_with_a_newer_entry_applies_only_that_entry,
            a_batch_that_skips_an_entry_returns_chain_gap_and_changes_nothing,
            a_diverged_chain_is_refused_and_is_not_a_gap,
            after_store_loss_a_gap_backfill_restores_the_rebuilt_totals,
            an_out_of_range_result_is_refused_and_changes_nothing,
            a_negative_delta_for_a_nonnegative_counter_is_refused,
            a_sequence_above_the_exact_lua_range_is_refused,
            every_input_rule_refuses_its_batch_as_malformed,
            a_signed_sum_is_range_checked_after_every_entry,
            jev_counts_and_overhead_sums_apply_under_the_same_identity_rule,
        );
        $crate::__contract_suite!(store, $crate::learn_store::contract::isolation, $attrs, $make;
            concurrent_batches_from_many_sessions_sum_exactly,
            overlapping_windows_in_any_order_credit_each_entry_once,
            a_session_counts_once_for_each_level_key_and_strategy,
            projects_share_no_state,
            epochs_share_no_state,
            read_makes_no_write,
            a_call_with_nothing_to_write_leaves_no_trace,
            read_returns_the_jev_counts_and_overhead_sums_of_the_turn,
            read_returns_every_requested_key_strategy_and_target_zero_filled_in_order,
        );
    };
}
