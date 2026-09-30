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
use roundhouse_core::routing::learn::{
    CacheReuse, EpochId, JevCounts, LatencySum, Strategy, Units,
};
use roundhouse_core::session::{Deltas, JevDelta, LearningEntry, QualityDelta, TargetDelta};
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

/// A batch whose write phase is larger than Lua's stack still applies whole.
///
/// Lua's `unpack` fails past about 8000 results, and the operations hash of
/// 700 targets is 4202 fields, 8404 `HSET` arguments. The credit entry before
/// it stages a quality hash, which the script writes first. A write phase that
/// unpacked each hash in one call would store that quality hash and then
/// raise on the operations hash, leaving counters behind with no watermark to
/// say they were applied.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_batch_past_the_lua_unpack_limit_applies_whole() {
    let store = connect_learner_in(fresh_namespace()).await;
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let targets: Vec<_> = (0..700).map(|i| frontier(&format!("m{i}"))).collect();
    let mut wide = residual(e, &targets[0], 0);
    wide.targets = targets
        .iter()
        .enumerate()
        .map(|(i, target)| {
            let mut row = residual(e, target, i as i64 - 350).targets.remove(0);
            row.failover = i as u64;
            row
        })
        .collect();
    let entries = [
        entry(10, 0, Some(credit(e, l0(&turn), Strategy::Rules, 3, 5))),
        entry(20, 10, Some(wide)),
    ];

    let applied = store.apply(&batch(&project, &session, &entries)).await;
    assert_eq!(
        applied,
        Ok(roundhouse_core::learn_store::Applied {
            applied: 2,
            watermark: 20
        })
    );
    let request = ReadRequest::new(project.clone(), e, &turn, &all_strategies(), &targets);
    let view = store.read(&request).await.unwrap();
    let rules = view.levels[2]
        .strategies
        .iter()
        .find(|counts| counts.strategy == Strategy::Rules)
        .unwrap();
    assert_eq!((rules.pos_units, rules.n_units, rules.sessions), (3, 5, 1));
    assert_eq!(view.targets.len(), 700);
    for (i, row) in view.targets.iter().enumerate() {
        assert_eq!(row.target, targets[i].policy_identity());
        assert_eq!(
            (row.latency.sum_ms, row.latency.n, row.failover),
            (i as i64 - 350, 1, i as u64),
            "{}",
            row.target
        );
    }
}

/// A stored value this store never writes is refused by `read` as
/// `Unavailable` naming its key and field: a negative count in an unsigned
/// field, and any text that is not a plain decimal integer.
///
/// `read` may fail only with `Unavailable` or `WrongType`, so it cannot use
/// `CounterRange`; but the message must still say where the foreign value is,
/// or the operator has nothing to clean up. A read that took the value as 0,
/// or as whatever Lua's `tonumber` makes of hex and exponents, would route on
/// a count nobody wrote.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_foreign_stored_value_is_refused_by_read_naming_its_key_and_field() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let mut raw = raw_from_env().await;
    let e = epoch(1);
    let turn = input(Tier::Capable, false);
    let target = frontier("large");
    let identity = target.policy_identity();
    let mut missed = Vec::new();
    // (quality key or not, field, stored value)
    let cases = [
        (true, "rules:pos".to_owned(), "-1"),
        (true, "capable:n".to_owned(), "1e3"),
        (true, "jev_capable".to_owned(), "0x10"),
        (false, format!("{identity}:lat_n"), "-1"),
        (false, format!("{identity}:lat_sum"), " 5"),
        (false, format!("{identity}:cache_n"), "abc"),
        (false, "turn:pre_sum".to_owned(), "2.5"),
    ];
    for (quality, field, value) in cases {
        let project = fresh_project();
        let key = if quality {
            learn_quality_key(&namespace, &project, e, l0(&turn))
        } else {
            learn_ops_key(&namespace, &project, e)
        };
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg(&field)
            .arg(value)
            .query_async(&mut raw)
            .await
            .unwrap();
        let request = ReadRequest::new(project, e, &turn, &all_strategies(), [&target]);
        match store.read(&request).await {
            Err(LearnerError::Unavailable(message))
                if message.contains(&key) && message.contains(&field) => {}
            other => missed.push(format!("{field} = {value:?}: read answered {other:?}")),
        }
    }
    assert!(missed.is_empty(), "{missed:#?}");
}

/// A watermark is a plain decimal integer within `2^53 - 1`, and `apply` and
/// `watermark` agree on that. Lua's `tonumber` reads `0x5` as 5 and Rust's
/// `u64` parse reads `+5` as 5; a store that let either through would chain
/// entries onto a watermark one reader sees and the other refuses.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_foreign_watermark_is_refused_by_apply_and_by_watermark() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let mut raw = raw_from_env().await;
    let session = SessionId::generate();
    let e = epoch(1);
    let turn = input(Tier::Capable, false);
    let mut missed = Vec::new();
    for value in ["0x5", "+5", "5.0", "-0", " 5", "1e1"] {
        let project = fresh_project();
        let key = learn_watermark_key(&namespace, &project);
        let _: () = redis::cmd("HSET")
            .arg(&key)
            .arg(session.as_str())
            .arg(value)
            .query_async(&mut raw)
            .await
            .unwrap();
        let before = store.snapshot(&project).await;
        let entries = [entry(
            10,
            0,
            Some(credit(e, l0(&turn), Strategy::Rules, 1, 1)),
        )];
        match store.apply(&batch(&project, &session, &entries)).await {
            Err(LearnerError::CounterRange { counter })
                if counter.contains(&key) && counter.contains(session.as_str()) => {}
            other => missed.push(format!("{value:?}: apply answered {other:?}")),
        }
        if store.snapshot(&project).await != before {
            missed.push(format!("{value:?}: apply wrote"));
        }
        // `WrongType`, not `Unavailable`: the field is one session's, and
        // the store answered (M9 review, M1).
        match store.watermark(&project, &session).await {
            Err(LearnerError::WrongType { key: named }) if named == key => {}
            other => missed.push(format!("{value:?}: watermark answered {other:?}")),
        }
    }
    assert!(missed.is_empty(), "{missed:#?}");
}

/// A snapshot and restore of one project touch no key of another project
/// whose id extends it. The `SCAN` pattern ends at the `}:` after the hash
/// tag; without it, `proj_ab*` matches `proj_ab12`'s keys too, and a restore
/// of `proj_ab` would roll `proj_ab12` back with it.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_restore_leaves_a_project_whose_id_extends_it_alone() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let (short, long) = (
        roundhouse_core::control::ProjectId::new("proj_ab"),
        roundhouse_core::control::ProjectId::new("proj_ab12"),
    );
    let session = SessionId::generate();
    let e = epoch(1);
    let turn = input(Tier::Capable, false);
    let first = [entry(
        10,
        0,
        Some(credit(e, l0(&turn), Strategy::Rules, 1, 1)),
    )];
    let second = [entry(
        20,
        10,
        Some(credit(e, l0(&turn), Strategy::Rules, 1, 2)),
    )];
    for project in [&short, &long] {
        store
            .apply(&batch(project, &session, &first))
            .await
            .unwrap();
    }

    let snapshot = store.snapshot(&short).await;
    let long_wm = learn_watermark_key(&namespace, &long);
    let long_prefix = &long_wm[..long_wm.len() - "wm".len()];
    let foreign: Vec<_> = snapshot
        .keys()
        .filter(|key| key.starts_with(long_prefix))
        .collect();
    assert!(
        foreign.is_empty(),
        "the snapshot of proj_ab holds {foreign:?}"
    );

    for project in [&short, &long] {
        store
            .apply(&batch(project, &session, &second))
            .await
            .unwrap();
    }
    let long_before = store.snapshot(&long).await;
    store.restore(&short, snapshot).await;

    assert_eq!(store.watermark(&short, &session).await, Ok(10));
    assert_eq!(store.watermark(&long, &session).await, Ok(20));
    assert_eq!(
        store.snapshot(&long).await,
        long_before,
        "proj_ab12 changed"
    );
}

/// Pins every stored field to its literal byte spelling, not just its
/// position in `TARGET_COUNTERS` or the quality hash's counter list.
///
/// The trait-level suite above only ever reads a field back through the same
/// table that wrote it, so a reorder of `TARGET_COUNTERS` — say `cache_obs`
/// before `cache_pred` — relabels durable data while every generated test
/// stays green: `apply` and `read` walk the reordered table together and
/// agree with each other, just not with what is already on disk under the
/// old order. One entry gives every field of one target's ops row and one
/// strategy's quality row a distinct value, and a raw `HGET` by the field's
/// exact name is the only way to catch a field landing under the wrong one.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn every_stored_field_is_pinned_by_its_literal_name() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let mut raw = raw_from_env().await;
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    let target = frontier("large");
    let identity = target.policy_identity();

    let deltas = Deltas {
        epoch: e,
        quality: vec![QualityDelta {
            key,
            strategy: Strategy::Rules,
            units: Units { pos: 11, n: 22 },
        }],
        targets: vec![TargetDelta {
            target: identity.clone(),
            latency: LatencySum { sum_ms: -33, n: 44 },
            failover: 55,
            cache: CacheReuse {
                predicted_permille: 66,
                observed_permille: 77,
                n: 88,
            },
        }],
        overhead: LatencySum { sum_ms: 99, n: 100 },
        jev: vec![JevDelta {
            key,
            counts: JevCounts {
                capable: 111,
                efficient: 122,
            },
        }],
    };
    store
        .apply(&batch(&project, &session, &[entry(10, 0, Some(deltas))]))
        .await
        .unwrap();

    let quality_key = learn_quality_key(&namespace, &project, e, key);
    let ops_key = learn_ops_key(&namespace, &project, e);
    let quality_fields = [
        ("rules:pos", 11i64),
        ("rules:n", 22),
        ("rules:sessions", 1),
        ("jev_capable", 111),
        ("jev_efficient", 122),
    ];
    let ops_fields = [
        (format!("{identity}:lat_sum"), -33i64),
        (format!("{identity}:lat_n"), 44),
        (format!("{identity}:failover"), 55),
        (format!("{identity}:cache_pred"), 66),
        (format!("{identity}:cache_obs"), 77),
        (format!("{identity}:cache_n"), 88),
        ("turn:pre_sum".to_owned(), 99),
        ("turn:pre_n".to_owned(), 100),
    ];
    let mut missed = Vec::new();
    for (field, expected) in quality_fields {
        let stored: Option<String> = redis::cmd("HGET")
            .arg(&quality_key)
            .arg(field)
            .query_async(&mut raw)
            .await
            .unwrap();
        if stored.as_deref() != Some(expected.to_string().as_str()) {
            missed.push(format!(
                "{quality_key} {field} = {stored:?}, expected {expected}"
            ));
        }
    }
    for (field, expected) in &ops_fields {
        let stored: Option<String> = redis::cmd("HGET")
            .arg(&ops_key)
            .arg(field)
            .query_async(&mut raw)
            .await
            .unwrap();
        if stored.as_deref() != Some(expected.to_string().as_str()) {
            missed.push(format!(
                "{ops_key} {field} = {stored:?}, expected {expected}"
            ));
        }
    }
    assert!(missed.is_empty(), "{missed:#?}");
}

/// A large counter is stored in plain decimal digits, not scientific
/// notation.
///
/// The apply script stores every counter as a Lua number computed by
/// arithmetic (`counter(...) + delta`), and it is Redis's Lua-to-RESP
/// argument conversion — not this crate's code — that turns that number into
/// the bytes `HSET` writes. The strict parser `read` and `watermark` both
/// apply accepts decimal digits alone, so a value Redis happened to write as
/// `1e+15` would come back foreign forever. Both counters here are under
/// `2^53 - 1`, the range this store promises exactly.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_large_counter_is_stored_in_plain_digits() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let mut raw = raw_from_env().await;
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let target = frontier("large");
    let identity = target.policy_identity();
    let (sum_ms, n): (i64, u64) = (1_000_000_000_000_000, 100_000_000);

    let deltas = Deltas {
        epoch: e,
        quality: Vec::new(),
        targets: vec![TargetDelta {
            target: identity.clone(),
            latency: LatencySum { sum_ms, n },
            failover: 0,
            cache: CacheReuse::default(),
        }],
        overhead: LatencySum::default(),
        jev: Vec::new(),
    };
    store
        .apply(&batch(&project, &session, &[entry(10, 0, Some(deltas))]))
        .await
        .unwrap();

    let request = ReadRequest::new(project.clone(), e, &turn, &all_strategies(), [&target]);
    let view = store.read(&request).await.unwrap();
    assert_eq!(view.targets[0].latency, LatencySum { sum_ms, n });

    let ops_key = learn_ops_key(&namespace, &project, e);
    for (field, expected) in [
        (format!("{identity}:lat_sum"), sum_ms.to_string()),
        (format!("{identity}:lat_n"), n.to_string()),
    ] {
        let stored: String = redis::cmd("HGET")
            .arg(&ops_key)
            .arg(&field)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(stored, expected, "{field}");
    }
}

/// **A non-UTF-8 watermark field is `WrongType`, not `Unavailable`** (M9
/// round-4, item 2, low). `watermark` used to decode the `HGET` reply
/// straight into a `String`; invalid UTF-8 bytes fail that decode with a
/// code-less client error, which the old code read as `Unavailable` -- the
/// recovery task's outage classification, for one session's foreign field.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_non_utf8_watermark_field_is_refused_as_wrong_type() {
    let namespace = fresh_namespace();
    let store = connect_learner_in(namespace.clone()).await;
    let mut raw = raw_from_env().await;
    let session = SessionId::generate();
    let project = fresh_project();
    let key = learn_watermark_key(&namespace, &project);
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg(session.as_str())
        .arg(vec![0xFFu8, 0xFE])
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(
        store.watermark(&project, &session).await,
        Err(LearnerError::WrongType { key }),
        "invalid UTF-8 is one session's foreign field, not the store being down"
    );
}
