// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The key bytes of the learner family, and the reply shape of its scripts.
//! The shared contract suite runs in `tests/learn_contract.rs`.

use redis::Value;

use roundhouse_core::control::ProjectId;
use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::contract::{
    batch, credit, entry, epoch, fresh_project, frontier, input, l0, residual,
};
use roundhouse_core::learn_store::{MAX_EXACT, ReadRequest};
use roundhouse_core::routing::Tier;
use roundhouse_core::routing::learn::{EpochId, Strategy, StrategySet};
use roundhouse_core::session::LearningEntry;

use super::{
    ApplyPlan, Arg, HASH, OP_SESSION, ReadPlan, RedisLearnerStore, SET, ops_key, project_prefix,
    quality_key, seen_key, stored_count, watermark_key,
};
use crate::keys::KeyNamespace;

/// Writer and reader both derive every key from these functions, so a byte
/// changing in one round-trips clean against every other test here and only
/// orphans what a running deployment already wrote. Pinned the way
/// `every_learning_index_key_is_pinned_by_byte` pins the source index.
#[test]
fn every_learn_key_is_pinned_by_byte() {
    let namespace = KeyNamespace::new("probe").unwrap();
    let project = ProjectId::new("acme");
    let e = EpochId::new([0xab; 16]);
    let turn = input(Tier::Capable, true);
    let [l2, l1, l0] = turn.keys();
    let hex = "abababababababababababababababab";
    assert_eq!(
        watermark_key(&namespace, &project),
        "probe:v1:learn:{acme}:wm"
    );
    assert_eq!(
        quality_key(&namespace, &project, e, l2),
        format!("probe:v1:learn:{{acme}}:{hex}:q:l2:capable.none.absent.tools")
    );
    assert_eq!(
        quality_key(&namespace, &project, e, l1),
        format!("probe:v1:learn:{{acme}}:{hex}:q:l1:capable.none")
    );
    assert_eq!(
        quality_key(&namespace, &project, e, l0),
        format!("probe:v1:learn:{{acme}}:{hex}:q:l0:capable")
    );
    assert_eq!(
        ops_key(&namespace, &project, e),
        format!("probe:v1:learn:{{acme}}:{hex}:ops")
    );
    assert_eq!(
        seen_key(&namespace, &project, e, &SessionId::new("sess_1")),
        format!("probe:v1:learn:{{acme}}:{hex}:seen:sess_1")
    );
    assert_eq!(
        project_prefix(&namespace, &project),
        "probe:v1:learn:{acme}:"
    );
}

/// Every key of one project starts with the project's prefix, so the
/// test-support snapshot's `SCAN` finds all of them, and carries one hash
/// tag, so one script can touch all of them if this family ever runs on a
/// Cluster.
#[test]
fn every_key_of_one_project_starts_with_its_prefix_and_shares_its_tag() {
    let namespace = KeyNamespace::default();
    let project = ProjectId::new("proj_a");
    let e = epoch(3);
    let turn = input(Tier::Efficient, false);
    let mut keys = vec![
        watermark_key(&namespace, &project),
        ops_key(&namespace, &project, e),
        seen_key(&namespace, &project, e, &SessionId::new("sess_x")),
    ];
    keys.extend(
        turn.keys()
            .into_iter()
            .map(|key| quality_key(&namespace, &project, e, key)),
    );
    let prefix = project_prefix(&namespace, &project);
    for key in &keys {
        assert!(key.starts_with(&prefix), "{key} is outside {prefix}");
        let tag = &key[key.find('{').unwrap()..=key.find('}').unwrap()];
        assert_eq!(tag, "{proj_a}", "{key}");
    }
}

/// Every reply of both scripts is an array of integers, whatever the outcome.
///
/// A string in a reply would be a number that crossed through `tostring`, or a
/// status tag the Rust side has to parse; a counter formatted with `%.14g`
/// loses digits past the fourteenth (draft section 11.3). Each case also
/// asserts the reply code, so the test reaches every code rather than
/// passing on the one outcome it happened to produce.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_scripts_return_only_integers() {
    let url = std::env::var("ROUNDHOUSE_TEST_REDIS_URL")
        .expect("--include-ignored asks for the real backend; set ROUNDHOUSE_TEST_REDIS_URL");
    let namespace = KeyNamespace::new(format!("rhtest-{}", uuid::Uuid::new_v4().simple())).unwrap();
    let store = RedisLearnerStore::connect_namespaced(&url, namespace.clone())
        .await
        .unwrap();
    let mut conn = store.connection();
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let key = l0(&turn);
    let strategies = StrategySet::new(vec![Strategy::Rules, Strategy::Capable]).unwrap();
    let target = frontier("large");
    let request = ReadRequest::new(project.clone(), e, &turn, &strategies, [&target]);
    let read_plan = ReadPlan::new(&namespace, &request);

    let apply = |entries: Vec<LearningEntry>| {
        let plan = ApplyPlan::new(&namespace, &batch(&project, &session, &entries));
        let mut conn = store.connection();
        let scripts = store.scripts.clone();
        async move { scripts.apply(&mut conn, &plan).await.unwrap() }
    };
    let applied = vec![
        entry(10, 0, Some(credit(e, key, Strategy::Rules, 3, 5))),
        entry(20, 10, Some(residual(e, &target, -7))),
    ];
    let over = vec![entry(
        30,
        20,
        Some(credit(e, key, Strategy::Rules, 0, MAX_EXACT)),
    )];
    let mut replies: Vec<(&str, i64, Vec<Value>)> = vec![
        ("apply", 0, apply(applied.clone()).await),
        ("skip", 0, apply(applied).await),
        ("gap", 2, apply(vec![entry(40, 30, None)]).await),
        ("diverged", 3, apply(vec![entry(30, 10, None)]).await),
        ("range", 4, apply(over).await),
        (
            "read",
            0,
            store.scripts.read(&mut conn, &read_plan).await.unwrap(),
        ),
    ];

    let _: () = redis::cmd("SET")
        .arg(ops_key(&namespace, &project, e))
        .arg("foreign")
        .query_async(&mut conn)
        .await
        .unwrap();
    replies.push((
        "read of a wrong type",
        1,
        store.scripts.read(&mut conn, &read_plan).await.unwrap(),
    ));
    let touches_ops = vec![entry(30, 20, Some(residual(e, &target, 1)))];
    replies.push(("apply to a wrong type", 1, apply(touches_ops).await));

    for (name, code, reply) in &replies {
        assert!(
            reply.iter().all(|value| matches!(value, Value::Int(_))),
            "{name}: {reply:?}"
        );
        assert_eq!(reply.first(), Some(&Value::Int(*code)), "{name}: {reply:?}");
    }
}

/// The `SADD` of the write phase is chunked like the `HSET`s.
///
/// Lua's `unpack` raises past about 8000 results, and the script writes the
/// counter hashes before the `seen` sets, so an unchunked `SADD` of 9000 new
/// members would fail after the quality hash was stored. No batch built from
/// real deltas names that many members, so the plan is built by hand: one
/// entry of 9000 session ops, each with its own member.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_seen_set_past_the_lua_unpack_limit_applies_whole() {
    let url = std::env::var("ROUNDHOUSE_TEST_REDIS_URL")
        .expect("--include-ignored asks for the real backend; set ROUNDHOUSE_TEST_REDIS_URL");
    let namespace = KeyNamespace::new(format!("rhtest-{}", uuid::Uuid::new_v4().simple())).unwrap();
    let store = RedisLearnerStore::connect_namespaced(&url, namespace.clone())
        .await
        .unwrap();
    let (project, session, e) = (fresh_project(), SessionId::generate(), epoch(1));
    let turn = input(Tier::Capable, false);
    let quality = quality_key(&namespace, &project, e, l0(&turn));
    let seen = seen_key(&namespace, &project, e, &session);
    const MEMBERS: i64 = 9000;
    let mut ops = Vec::new();
    for member in 0..MEMBERS {
        ops.extend([
            Arg::Int(OP_SESSION),
            Arg::Int(3),
            Arg::Text(format!("m{member}")),
            Arg::Int(2),
            Arg::Text("rules:sessions".to_owned()),
        ]);
    }
    let mut args = vec![
        Arg::Text(session.as_str().to_owned()),
        Arg::Text(format!("{HASH}{HASH}{SET}")),
        Arg::Int(1),
        Arg::Int(10),
        Arg::Int(0),
        Arg::Int(ops.len() as i64),
    ];
    args.extend(ops);
    let plan = ApplyPlan {
        keys: vec![
            watermark_key(&namespace, &project),
            quality.clone(),
            seen.clone(),
        ],
        args,
    };

    let reply = store
        .scripts
        .apply(&mut store.connection(), &plan)
        .await
        .unwrap();
    assert_eq!(
        reply,
        vec![Value::Int(0), Value::Int(1), Value::Int(10)],
        "one entry applied"
    );
    let mut conn = store.connection();
    let members: i64 = redis::cmd("SCARD")
        .arg(&seen)
        .query_async(&mut conn)
        .await
        .unwrap();
    let sessions: Option<i64> = redis::cmd("HGET")
        .arg(&quality)
        .arg("rules:sessions")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!((members, sessions), (MEMBERS, Some(MEMBERS)));
}

/// The same rule `watermark` applies to a stored field: decimal digits alone,
/// within `2^53 - 1`. No real Redis needed, unlike every other test in this
/// file: this is a pure parse.
#[test]
fn stored_count_accepts_only_a_plain_digit_string_within_max_exact() {
    assert_eq!(stored_count("5"), Some(5));
    assert_eq!(stored_count("+5"), None);
    assert_eq!(stored_count("0x5"), None);
    assert_eq!(stored_count(""), None);
    assert_eq!(
        stored_count("9007199254740991"),
        Some(9_007_199_254_740_991)
    );
    assert_eq!(stored_count("9007199254740992"), None);
}
