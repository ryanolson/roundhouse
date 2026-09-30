// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The read and isolation half of the [`LearnerStore`] contract: concurrency,
//! session counting, project and epoch isolation, and what a read returns.
//!
//! Listed in
//! [`learner_store_contract_suite!`](crate::learner_store_contract_suite)
//! beside the identity cases of the parent module.

use futures::future::join_all;

use crate::ids::SessionId;
use crate::learn_store::{LearnerError, ReadRequest};
use crate::routing::Tier;
use crate::routing::learn::{
    CacheReuse, JevCounts, LatencySum, LevelView, ReadView, Strategy, StrategyCounts, StrategySet,
    TargetOps,
};
use crate::session::{Deltas, TargetDelta};

use super::{
    LearnerStoreControl, batch, chain, counts, credit, entry, epoch, fresh_project, frontier,
    input, jev_and_overhead, l0, read_view,
};

/// Many sessions of one project apply at once to the same key. Integer
/// addition commutes and each apply is atomic, so no update is lost.
pub async fn concurrent_batches_from_many_sessions_sum_exactly<S: LearnerStoreControl>(store: &S) {
    const SESSIONS: usize = 32;
    const ENTRIES: usize = 5;
    let (project, e) = (fresh_project(), epoch(1));
    let turn = input(Tier::Capable, false);

    join_all((0..SESSIONS).map(|_| {
        let (project, turn) = (project.clone(), turn);
        async move {
            let session = SessionId::generate();
            for entry in chain(e, &turn, ENTRIES) {
                store
                    .apply(&batch(&project, &session, &[entry]))
                    .await
                    .expect("each session's chain applies in order");
            }
        }
    }))
    .await;

    let counts = counts(store, &project, e, &turn, l0(&turn), Strategy::Rules).await;
    assert_eq!(counts.n_units, SESSIONS as u64 * 0b11111);
    assert_eq!(counts.sessions, SESSIONS as u64);
}

/// A deterministic generator, so a failing trial names a reproducible seed
/// and the suite needs no dependency.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % bound as u64) as usize
    }
}

/// Windows of one chain sent in shuffled order, as overlapping resends from
/// several nodes and the recovery task would send them, then one full pass.
/// Every entry counts exactly once. The chain credits `1 << index`, so a lost
/// or doubled entry changes the sum.
pub async fn overlapping_windows_in_any_order_credit_each_entry_once<S: LearnerStoreControl>(
    store: &S,
) {
    const TRIALS: u64 = 40;
    const LEN: usize = 8;
    const WINDOWS: usize = 6;
    let e = epoch(1);
    let turn = input(Tier::Capable, false);
    let entries = chain(e, &turn, LEN);

    for trial in 0..TRIALS {
        let mut rng = Lcg(trial);
        let (project, session) = (fresh_project(), SessionId::generate());
        let mut windows: Vec<(usize, usize)> = (0..WINDOWS)
            .map(|_| {
                let start = rng.below(LEN);
                (start, start + 1 + rng.below(LEN - start))
            })
            .collect();
        for index in (1..windows.len()).rev() {
            windows.swap(index, rng.below(index + 1));
        }
        for (start, end) in windows {
            let sent = batch(&project, &session, &entries[start..end]);
            match store.apply(&sent).await {
                Ok(_) | Err(LearnerError::ChainGap { .. }) => {}
                Err(error) => panic!("trial {trial}: window {start}..{end}: {error}"),
            }
        }
        store
            .apply(&batch(&project, &session, &entries))
            .await
            .unwrap_or_else(|error| panic!("trial {trial}: the full pass applies: {error}"));

        let counts = counts(store, &project, e, &turn, l0(&turn), Strategy::Rules).await;
        assert_eq!(
            counts.n_units,
            (1 << LEN) - 1,
            "trial {trial}: each entry credited once"
        );
        assert_eq!(counts.sessions, 1, "trial {trial}");
        assert_eq!(
            store.watermark(&project, &session).await.unwrap(),
            10 * LEN as u64
        );
    }
}

/// `sessions` counts a session once for each key and strategy, however many
/// intervals it credits there: within one batch, across batches, and across a
/// resend.
pub async fn a_session_counts_once_for_each_level_key_and_strategy<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, e) = (fresh_project(), epoch(1));
    let turn = input(Tier::Capable, false);
    let [l2, _, l0] = turn.keys();
    let first = SessionId::generate();
    let one_batch = vec![
        entry(10, 0, Some(credit(e, l0, Strategy::Rules, 1, 1))),
        entry(20, 10, Some(credit(e, l0, Strategy::Rules, 1, 1))),
        entry(30, 20, Some(credit(e, l2, Strategy::Rules, 1, 1))),
        entry(40, 30, Some(credit(e, l0, Strategy::Capable, 1, 1))),
    ];
    store
        .apply(&batch(&project, &first, &one_batch))
        .await
        .unwrap();
    let later = entry(50, 40, Some(credit(e, l0, Strategy::Rules, 1, 1)));
    store
        .apply(&batch(&project, &first, std::slice::from_ref(&later)))
        .await
        .unwrap();
    let mut resend = one_batch;
    resend.push(later);
    store
        .apply(&batch(&project, &first, &resend))
        .await
        .unwrap();

    let sessions = |counts: StrategyCounts| (counts.n_units, counts.sessions);
    assert_eq!(
        sessions(counts(store, &project, e, &turn, l0, Strategy::Rules).await),
        (3, 1),
        "three intervals of one session at one key and strategy"
    );
    assert_eq!(
        sessions(counts(store, &project, e, &turn, l2, Strategy::Rules).await),
        (1, 1),
        "the same session at another key counts there"
    );
    assert_eq!(
        sessions(counts(store, &project, e, &turn, l0, Strategy::Capable).await),
        (1, 1),
        "and for another strategy"
    );

    let second = SessionId::generate();
    store
        .apply(&batch(
            &project,
            &second,
            &[entry(10, 0, Some(credit(e, l0, Strategy::Rules, 1, 1)))],
        ))
        .await
        .unwrap();
    assert_eq!(
        sessions(counts(store, &project, e, &turn, l0, Strategy::Rules).await),
        (4, 2),
        "a second session is a second session"
    );
}

/// Two projects with the same session id, epoch and keys hold separate
/// counters and separate watermarks.
pub async fn projects_share_no_state<S: LearnerStoreControl>(store: &S) {
    let (acme, globex, e) = (fresh_project(), fresh_project(), epoch(1));
    let session = SessionId::generate();
    let turn = input(Tier::Capable, false);
    store
        .apply(&batch(&acme, &session, &chain(e, &turn, 3)))
        .await
        .unwrap();

    assert_eq!(store.watermark(&globex, &session).await.unwrap(), 0);
    let untouched = counts(store, &globex, e, &turn, l0(&turn), Strategy::Rules).await;
    assert_eq!((untouched.n_units, untouched.sessions), (0, 0));

    let first = chain(e, &turn, 1);
    let applied = store
        .apply(&batch(&globex, &session, &first))
        .await
        .expect("another project's watermark does not skip this entry");
    assert_eq!(applied.applied, 1);
    let globex_counts = counts(store, &globex, e, &turn, l0(&turn), Strategy::Rules).await;
    assert_eq!((globex_counts.n_units, globex_counts.sessions), (1, 1));
    let acme_counts = counts(store, &acme, e, &turn, l0(&turn), Strategy::Rules).await;
    assert_eq!((acme_counts.n_units, acme_counts.sessions), (0b111, 1));
}

/// Two epochs of one project hold separate counters and count a session in
/// each. The watermark is per session, not per epoch: a session whose entries
/// cross an epoch change is one chain.
pub async fn epochs_share_no_state<S: LearnerStoreControl>(store: &S) {
    let (project, session) = (fresh_project(), SessionId::generate());
    let (old, new) = (epoch(1), epoch(2));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    // Two batches, so the second epoch's session count is decided against
    // the `seen` set the first batch stored, not only the batch's own.
    let entries = vec![
        entry(10, 0, Some(credit(old, key, Strategy::Rules, 1, 1))),
        entry(20, 10, Some(credit(new, key, Strategy::Rules, 2, 4))),
        entry(
            30,
            20,
            Some(jev_and_overhead(
                new,
                &turn,
                JevCounts {
                    capable: 1,
                    efficient: 0,
                },
                9,
            )),
        ),
    ];
    store
        .apply(&batch(&project, &session, &entries[..1]))
        .await
        .unwrap();
    store
        .apply(&batch(&project, &session, &entries))
        .await
        .unwrap();

    let old_counts = counts(store, &project, old, &turn, key, Strategy::Rules).await;
    let new_counts = counts(store, &project, new, &turn, key, Strategy::Rules).await;
    assert_eq!(
        (
            old_counts.pos_units,
            old_counts.n_units,
            old_counts.sessions
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            new_counts.pos_units,
            new_counts.n_units,
            new_counts.sessions
        ),
        (2, 4, 1),
        "the session counts once in each epoch"
    );
    let old_view = read_view(store, &project, old, &turn).await;
    assert_eq!(old_view.overhead, LatencySum::default());
    assert!(
        old_view
            .levels
            .iter()
            .all(|level| level.jev == JevCounts::default())
    );
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 30);
}

/// A read changes nothing a snapshot can see: no counter, no watermark, no
/// empty key created for a turn whose keys hold nothing yet.
pub async fn read_makes_no_write<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    store
        .apply(&batch(&project, &session, &chain(e, &turn, 2)))
        .await
        .unwrap();
    let before = store.snapshot(&project).await;

    read_view(store, &project, e, &turn).await;
    read_view(store, &project, e, &input(Tier::Efficient, true)).await;
    read_view(store, &project, epoch(9), &turn).await;
    store
        .watermark(&project, &SessionId::generate())
        .await
        .unwrap();

    assert_eq!(store.snapshot(&project).await, before);
}

/// A call with nothing to write leaves no trace, even of the project: a read
/// and a watermark query of a project the store holds nothing for, and an
/// empty batch, leave it absent. On a project that exists, an empty batch for
/// a session the store has never seen writes no watermark for it, and a batch
/// wholly at or below the watermark changes nothing (that last case is a
/// control: its write phase, if it ran, would rewrite the values it read).
pub async fn a_call_with_nothing_to_write_leaves_no_trace<S: LearnerStoreControl>(store: &S) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let absent = store.snapshot(&project).await;

    read_view(store, &project, e, &turn).await;
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 0);
    assert_eq!(
        store.snapshot(&project).await,
        absent,
        "a read of an absent project creates it"
    );
    let empty = store.apply(&batch(&project, &session, &[])).await.unwrap();
    assert_eq!((empty.applied, empty.watermark), (0, 0));
    assert_eq!(
        store.snapshot(&project).await,
        absent,
        "an empty batch creates the project"
    );

    let entries = chain(e, &turn, 2);
    store
        .apply(&batch(&project, &session, &entries))
        .await
        .unwrap();
    let before = store.snapshot(&project).await;
    let newcomer = SessionId::generate();
    let empty = store.apply(&batch(&project, &newcomer, &[])).await.unwrap();
    assert_eq!((empty.applied, empty.watermark), (0, 0));
    assert_eq!(
        store.snapshot(&project).await,
        before,
        "an empty batch writes a watermark for a new session"
    );
    let skipped = store
        .apply(&batch(&project, &session, &entries))
        .await
        .unwrap();
    assert_eq!((skipped.applied, skipped.watermark), (0, 20));
    assert_eq!(store.snapshot(&project).await, before);
}

/// The Jev counts of each key of the turn, and the project's overhead sum,
/// come back on the read. Answers on a key of another turn do not.
pub async fn read_returns_the_jev_counts_and_overhead_sums_of_the_turn<S: LearnerStoreControl>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let other = input(Tier::Efficient, true);
    store
        .apply(&batch(
            &project,
            &session,
            &[
                entry(
                    10,
                    0,
                    Some(jev_and_overhead(
                        e,
                        &turn,
                        JevCounts {
                            capable: 2,
                            efficient: 0,
                        },
                        30,
                    )),
                ),
                entry(
                    20,
                    10,
                    Some(jev_and_overhead(
                        e,
                        &turn,
                        JevCounts {
                            capable: 0,
                            efficient: 1,
                        },
                        -4,
                    )),
                ),
                entry(
                    30,
                    20,
                    Some(jev_and_overhead(
                        e,
                        &other,
                        JevCounts {
                            capable: 0,
                            efficient: 7,
                        },
                        11,
                    )),
                ),
            ],
        ))
        .await
        .unwrap();

    let view = read_view(store, &project, e, &turn).await;
    let jev: Vec<JevCounts> = view.levels.iter().map(|level| level.jev).collect();
    assert_eq!(
        jev,
        vec![
            JevCounts {
                capable: 2,
                efficient: 1
            };
            3
        ],
        "each key of the turn, and none of the other turn's"
    );
    assert_eq!(
        view.overhead,
        LatencySum { sum_ms: 37, n: 3 },
        "the overhead sum is the project's, over every turn"
    );
}

/// The exact shape of a read: every key of the turn most specific first, every
/// requested strategy in configured order, every requested target once in
/// first-occurrence order, zeros where the store holds nothing. The view is
/// recorded on every learned `Routed`, so a backend that dropped empty rows
/// would write a different log for the same state.
pub async fn read_returns_every_requested_key_strategy_and_target_zero_filled_in_order<
    S: LearnerStoreControl,
>(
    store: &S,
) {
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, true);
    let [l2, l1, l0] = turn.keys();
    let (large, small) = (frontier("large"), frontier("small"));
    let ops = Deltas {
        targets: vec![TargetDelta {
            target: small.policy_identity(),
            latency: LatencySum { sum_ms: 80, n: 2 },
            failover: 1,
            cache: CacheReuse {
                predicted_permille: 900,
                observed_permille: 450,
                n: 1,
            },
        }],
        ..credit(e, l1, Strategy::Capable, 3, 5)
    };
    store
        .apply(&batch(&project, &session, &[entry(10, 0, Some(ops))]))
        .await
        .unwrap();

    let strategies = StrategySet::new(vec![Strategy::Capable, Strategy::Rules]).unwrap();
    let request = ReadRequest::new(
        project.clone(),
        e,
        &turn,
        &strategies,
        [&large, &small, &large],
    );
    let view = store.read(&request).await.unwrap();

    let zero = |strategy| StrategyCounts {
        strategy,
        pos_units: 0,
        n_units: 0,
        sessions: 0,
    };
    let level = |key, strategies| LevelView {
        key,
        strategies,
        jev: JevCounts::default(),
    };
    let expected = ReadView {
        levels: vec![
            level(l2, vec![zero(Strategy::Capable), zero(Strategy::Rules)]),
            level(
                l1,
                vec![
                    StrategyCounts {
                        strategy: Strategy::Capable,
                        pos_units: 3,
                        n_units: 5,
                        sessions: 1,
                    },
                    zero(Strategy::Rules),
                ],
            ),
            level(l0, vec![zero(Strategy::Capable), zero(Strategy::Rules)]),
        ],
        targets: vec![
            TargetOps {
                target: large.policy_identity(),
                latency: LatencySum::default(),
                failover: 0,
                cache: CacheReuse::default(),
            },
            TargetOps {
                target: small.policy_identity(),
                latency: LatencySum { sum_ms: 80, n: 2 },
                failover: 1,
                cache: CacheReuse {
                    predicted_permille: 900,
                    observed_permille: 450,
                    n: 1,
                },
            },
        ],
        overhead: LatencySum::default(),
    };
    assert_eq!(view, expected);
}
