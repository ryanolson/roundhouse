// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M7 of the online routing learner: the full [`LearnerStore`] contract
//! against a real Redis, plus the cases only a real backend can exercise.
//!
//! The macro invocation is the headline, in the idiom of `contract.rs` and
//! `spend_contract.rs`: the same assertions that judge `MemoryLearnerStore`
//! judge this store, and a case added to the macro is added here with no
//! wiring step. Each generated test connects its own store under a fresh
//! namespace; the suite's cases mint fresh projects as well.
//!
//! Gating is the same as every other file in this crate's `tests/`:
//! `#[ignore]`, opted into with `--include-ignored`, and a missing
//! `ROUNDHOUSE_TEST_REDIS_URL` fails loudly rather than skipping quietly.
//!
//! [`LearnerStore`]: roundhouse_core::learn_store::LearnerStore

mod common;

use std::collections::BTreeSet;

use common::raw_from_env;
use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::contract::{
    LearnerStoreControl, all_strategies, batch, credit, entry, epoch, fresh_project, frontier,
    input, jev_and_overhead, l0, residual,
};
use roundhouse_core::learn_store::{LearnerError, LearnerStore, ReadRequest};
use roundhouse_core::routing::Tier;
use roundhouse_core::routing::learn::{EpochId, JevCounts, Strategy};
use roundhouse_core::session::LearningEntry;
use roundhouse_store_redis::test_support::{
    connect_learner_in, fresh_namespace, keys_in, learn_ops_key, learn_quality_key, learn_seen_key,
    learn_watermark_key,
};

roundhouse_core::learner_store_contract_suite!(
    ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored",
    connect_learner_in(fresh_namespace()).await
);

/// A batch that touches the watermark, all three quality keys of one turn
/// (credit at the L0 key, Jev counts at every key), the operations key, and
/// the session's `seen` set.
fn every_key_kind(e: EpochId) -> Vec<LearningEntry> {
    let turn = input(Tier::Capable, false);
    vec![
        entry(10, 0, Some(credit(e, l0(&turn), Strategy::Rules, 1, 2))),
        entry(20, 10, Some(residual(e, &frontier("large"), 5))),
        entry(
            30,
            20,
            Some(jev_and_overhead(
                e,
                &turn,
                JevCounts {
                    capable: 1,
                    efficient: 0,
                },
                8,
            )),
        ),
    ]
}

/// The live half of the key-builder convention, and the proof that the
/// test-support snapshot is not vacuous.
///
/// After a batch that touches every kind of key, the store's namespace holds
/// exactly the keys the family's key functions name, which the source check in
/// `key_builder_convention.rs` holds to `build_key`: the scripts wrote no key
/// of their own. The snapshot holds the same keys, so a `SCAN` pattern that
/// matched nothing (and so passed every "changed nothing" case of the suite)
/// fails here. Restoring the absent project's snapshot, the empty key set,
/// deletes every key.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_store_writes_only_the_keys_its_key_functions_name() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let absent = store.snapshot(&project).await;
    assert!(absent.is_empty(), "an absent project is the empty key set");

    store
        .apply(&batch(&project, &session, &every_key_kind(e)))
        .await
        .unwrap();

    let turn = input(Tier::Capable, false);
    let mut expected: BTreeSet<String> = turn
        .keys()
        .into_iter()
        .map(|key| learn_quality_key(&namespace, &project, e, key))
        .collect();
    expected.extend([
        learn_watermark_key(&namespace, &project),
        learn_ops_key(&namespace, &project, e),
        learn_seen_key(&namespace, &project, e, &session),
    ]);
    let written: BTreeSet<String> = keys_in(&namespace).await.into_iter().collect();
    assert_eq!(written, expected, "every key the store wrote");
    let snapshot = store.snapshot(&project).await;
    assert_eq!(
        snapshot.keys().cloned().collect::<BTreeSet<_>>(),
        expected,
        "the snapshot holds every key of the project"
    );

    store.restore(&project, absent).await;
    assert_eq!(keys_in(&namespace).await, Vec::<String>::new());
}

/// Draft section 11.3: a key of the wrong type is refused as `WrongType`,
/// naming the key, before the script writes anything.
///
/// Each case sabotages one key a batch or read touches, with the other keys
/// of the batch absent or holding counters, and takes the snapshot after the
/// sabotage. The `seen` set of epoch 2 is the last key the apply stages,
/// after counter hashes of both epochs, so a script that wrote as it went
/// would leave those counters behind. The reads meet the same refusal:
/// `read` on a quality or operations hash, `watermark` on the watermark hash.
///
/// The type check is what turns the refusal into `WrongType`. Without it the
/// check phase still stops before the write phase, because `redis.call`
/// raises on the first command against the sabotaged key, but the error
/// arrives as `Unavailable`, which the engine would retry forever.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_wrong_type_key_is_refused_before_any_write() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let mut raw = raw_from_env().await;
    let session = SessionId::generate();
    let e = epoch(1);
    let turn = input(Tier::Capable, false);
    let target = frontier("large");
    let read_request = |project| ReadRequest::new(project, e, &turn, &all_strategies(), [&target]);
    let mut missed = Vec::new();

    // One case per key kind: which key is sabotaged, and the command that
    // writes another type there.
    let cases = [
        ("seen set", "SET"),
        ("operations hash", "RPUSH"),
        ("quality hash", "SADD"),
        ("watermark hash", "SET"),
    ];
    for (kind, sabotage) in cases {
        let project = fresh_project();
        store
            .apply(&batch(&project, &session, &every_key_kind(e)[..1]))
            .await
            .unwrap();
        let key = match kind {
            "seen set" => learn_seen_key(&namespace, &project, epoch(2), &session),
            "operations hash" => learn_ops_key(&namespace, &project, e),
            "quality hash" => learn_quality_key(&namespace, &project, e, turn.keys()[0]),
            _ => learn_watermark_key(&namespace, &project),
        };
        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut raw)
            .await
            .unwrap();
        let _: () = redis::cmd(sabotage)
            .arg(&key)
            .arg("x")
            .query_async(&mut raw)
            .await
            .unwrap();
        let before = store.snapshot(&project).await;
        let wrong = LearnerError::WrongType { key: key.clone() };

        // Entry 20 credits a quality key and the operations key of epoch 1,
        // entry 30 those of epoch 2, and last the `seen` set of epoch 2.
        let mut first = credit(e, turn.keys()[0], Strategy::Capable, 1, 1);
        first.targets = residual(e, &target, 3).targets;
        let mut second = credit(epoch(2), l0(&turn), Strategy::Rules, 1, 1);
        second.targets = residual(epoch(2), &target, 4).targets;
        let entries = [entry(20, 10, Some(first)), entry(30, 20, Some(second))];
        let applied = store.apply(&batch(&project, &session, &entries)).await;
        if applied.as_ref().err() != Some(&wrong) {
            missed.push(format!("{kind}: apply answered {applied:?}"));
        }
        let after_apply = store.snapshot(&project).await;
        if after_apply != before {
            missed.push(format!("{kind}: apply wrote before its refusal"));
        }
        let read = store.read(&read_request(project.clone())).await;
        let watermark = store.watermark(&project, &session).await;
        match kind {
            "operations hash" | "quality hash" if read.as_ref().err() != Some(&wrong) => {
                missed.push(format!("{kind}: read answered {read:?}"));
            }
            "watermark hash" if watermark.as_ref().err() != Some(&wrong) => {
                missed.push(format!("{kind}: watermark answered {watermark:?}"));
            }
            _ => {}
        }
        if store.snapshot(&project).await != after_apply {
            missed.push(format!("{kind}: a read wrote"));
        }
    }
    assert!(missed.is_empty(), "{missed:#?}");
}
