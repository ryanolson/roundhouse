// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The memory learner store's conformance run, plus the cases of its
//! test-support instruments: the fault between the phases, the keys a read
//! visits, and when each instrument is armed.

use super::MemoryLearnerStore;
use crate::ids::SessionId;
use crate::learn_store::contract::{
    LearnerStoreControl, batch, chain, credit, entry, epoch, fresh_project, input, read_view,
};
use crate::learn_store::{LearnerError, LearnerStore};
use crate::routing::Tier;
use crate::routing::learn::{Band, KeyLevel, LevelKey, PriorBand, Strategy};

crate::learner_store_contract_suite!(MemoryLearnerStore::new());

/// Draft section 11.3: a failure after the check phase and before the write
/// phase leaves no change. The hook fails the one call at exactly that point;
/// a store that wrote while it checked would leave the first entries behind.
#[tokio::test]
async fn a_fault_between_check_and_write_changes_nothing() {
    let store = MemoryLearnerStore::new();
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    store
        .apply(&batch(&project, &session, &chain(e, &turn, 2)))
        .await
        .unwrap();
    let before = store.snapshot(&project).await;

    let entries = chain(e, &turn, 4);
    let next = batch(&project, &session, &entries);
    store.fail_next_apply_between_phases();
    assert!(matches!(
        store.apply(&next).await,
        Err(LearnerError::Unavailable(_))
    ));
    assert_eq!(store.snapshot(&project).await, before);
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 20);

    let retried = store.apply(&next).await.expect("the hook fails one call");
    assert_eq!((retried.applied, retried.watermark), (2, 40));
}

/// The armed fault fires on the next apply that has something to write. An
/// apply the check refuses, or one that skips every entry, passes it by, so a
/// test that arms it cannot have it spent by a call that never reaches the
/// point it guards.
#[tokio::test]
async fn the_fault_hook_fires_on_the_next_apply_that_would_write() {
    let store = MemoryLearnerStore::new();
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let entries = chain(e, &turn, 3);
    store
        .apply(&batch(&project, &session, &entries[..2]))
        .await
        .unwrap();
    store.fail_next_apply_between_phases();

    let skipped = store
        .apply(&batch(&project, &session, &entries[..2]))
        .await
        .expect("a wholly skipped batch writes nothing, so the fault waits");
    assert_eq!((skipped.applied, skipped.watermark), (0, 20));
    let gapped = [entry(40, 30, None)];
    assert_eq!(
        store.apply(&batch(&project, &session, &gapped)).await,
        Err(LearnerError::ChainGap {
            store_watermark: 20
        }),
        "a refused batch never reaches the fault"
    );
    assert_eq!(
        store
            .apply(&batch(&project, &session, &entries[2..]))
            .await
            .map(|applied| applied.applied),
        Err(LearnerError::Unavailable(
            "injected fault between the check and the write".to_owned()
        )),
        "the first apply with an entry to write meets the fault"
    );
    assert_eq!(store.watermark(&project, &session).await.unwrap(), 20);
}

/// Every level key the fields can spell, reachable or not: the store does not
/// know which are reachable, and more keys make the point better.
fn every_key() -> Vec<LevelKey> {
    let tiers = [Tier::Capable, Tier::Efficient];
    let bands = [Band::None, Band::Unknown, Band::Low, Band::High];
    let priors = [PriorBand::Absent, PriorBand::NoHigh, PriorBand::SomeHigh];
    let mut keys = Vec::new();
    for rules_pick in tiers {
        keys.push(LevelKey::L0 { rules_pick });
        for newest in bands {
            keys.push(LevelKey::L1 { rules_pick, newest });
            for prior in priors {
                for tool_turn in [false, true] {
                    keys.push(LevelKey::L2 {
                        rules_pick,
                        newest,
                        prior,
                        tool_turn,
                    });
                }
            }
        }
    }
    keys
}

/// A read visits the three quality keys of the turn and the operations key,
/// and nothing else, however many keys the project holds (draft section 11.4:
/// its cost does not depend on the number of keys or sessions).
#[tokio::test]
async fn read_visits_only_the_keys_of_the_turn() {
    const KEYS: usize = 1_000;
    let store = MemoryLearnerStore::new();
    let (project, session) = (fresh_project(), SessionId::generate());
    let keys = every_key();
    let entries: Vec<_> = (0..KEYS)
        .map(|index| {
            let seq = index as u64 + 1;
            let e = epoch((index / keys.len()) as u8);
            entry(
                seq,
                seq - 1,
                Some(credit(e, keys[index % keys.len()], Strategy::Rules, 1, 1)),
            )
        })
        .collect();
    let applied = store
        .apply(&batch(&project, &session, &entries))
        .await
        .unwrap();
    assert_eq!(applied.applied, KEYS);
    store.record_visits();

    let e = epoch(3);
    let turn = input(Tier::Efficient, true);
    let view = read_view(&store, &project, e, &turn).await;
    assert!(
        view.levels
            .iter()
            .all(|level| level.strategies[0].n_units == 1),
        "every key of the turn is populated in this epoch, so the read found them"
    );

    let expected: Vec<String> = turn
        .keys()
        .iter()
        .map(|key| format!("{e}:q:{}:{}", KeyLevel::label(key.level()), key.part()))
        .chain([format!("{e}:ops")])
        .collect();
    assert_eq!(store.take_visited(), expected);
}

/// The visit recorder is off until a test arms it. A store built with test
/// support and used by a test that never reads its visits must not grow a
/// list on every read.
#[tokio::test]
async fn an_unarmed_store_records_no_visits() {
    let store = MemoryLearnerStore::new();
    let (project, e) = (fresh_project(), epoch(1));
    read_view(&store, &project, e, &input(Tier::Capable, false)).await;
    read_view(&store, &project, e, &input(Tier::Efficient, true)).await;
    assert_eq!(store.take_visited(), Vec::<String>::new());
}
