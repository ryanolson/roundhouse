// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner at boot (milestone M9 of
//! `agent-docs/PLAN-online-routing-learner.md`): what `routing_composition`
//! builds from a booted plane and the backends `shared_backend::open` chose.
//!
//! **Every test calls the library functions `main.rs` calls**, never a copy of
//! their rules, for the reason `shared_backend` gives: a `[[bin]]` is not
//! something a test can call, and a rule re-typed here would stay green while
//! the real one changed. No test here initializes the binary's global logging;
//! `captured_warnings` owns the one global subscriber of this test binary.

use std::num::NonZeroUsize;
use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::learn_store::LearnerStore;
use roundhouse_core::metrics::MetricsConfig;
use roundhouse_core::routing::{DecisionRecord, SelectorBranch};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{EchoFrontierClient, FrontierClients, StaticFrontierCatalog, WireProtocol};
use roundhouse_server::control_config::{ControlPlaneError, learner::LearnerConfigError};
use roundhouse_server::routing_composition::{self, RoutingComposition};
use roundhouse_server::test_support::{captured_warnings, frontier_spec, single_model_catalog};
use roundhouse_server::{
    Admission, Backends, ControlPlane, ControlPlaneConfig, EchoLocalExecutor, Engine, EngineConfig,
    shared_backend,
};
use roundhouse_store_redis::KeyNamespace;

const TURN_HASH: &str = "0bd5182863262c911d4479f1b25fec5f3e6846653b9028e65f61b2b33677ddfd";

fn catalog() -> StaticFrontierCatalog {
    single_model_catalog(frontier_spec("echo", "echo", WireProtocol::OpenAiResponses))
}

fn artifact_file(bytes: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "roundhouse-learner-startup-{}.json",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, bytes).expect("the temp dir is writable");
    path.to_string_lossy().into_owned()
}

fn valid_artifact() -> String {
    artifact_file(
        r#"{"schema_revision":1,"input_revision":1,"selector_revision":1,"stage_revision":1,"credit_revision":1,"gate":"wilson-v1","strategies":["rules","capable"],"prior":[],"manifest_digest":"00","source_commit":"test"}"#,
    )
}

fn learner(mode: &str, artifact: String) -> serde_json::Value {
    serde_json::json!({
        "mode": mode,
        "strategies": ["rules", "capable"],
        "artifact": artifact,
        "quality": { "floor": 0.8, "z": 1.96, "min_evidence": 5000, "min_sessions": 20 },
        "latency_limit_ms": 10000,
        "latency_min_samples": 20,
        "cache_min_samples": 20,
        "read_timeout_ms": 2000,
        "apply_timeout_ms": 2000
    })
}

/// The section 4 starting values, with a 1 ms idle window so a test waits
/// only a few milliseconds for a mark to count as idle.
fn recovery_block() -> serde_json::Value {
    serde_json::json!({
        "sweep_interval_ms": 30000,
        "idle_after_ms": 1,
        "max_sessions_per_sweep": 64,
        "pages_per_session_per_sweep": 4,
        "audit_sessions_per_sweep": 32,
        "read_timeout_ms": 2000,
        "apply_timeout_ms": 2000,
        "source_timeout_ms": 2000
    })
}

/// How project `acme` is written in a document.
#[derive(Clone)]
enum Acme {
    Plain,
    Tiers,
    Learner(serde_json::Value),
}

/// Project `acme` as `acme` says, user `ada`, one turn key, and `recovery`.
fn document(acme: Acme, recovery: bool) -> String {
    let mut project = serde_json::json!({ "id": "acme" });
    match acme {
        Acme::Plain => {}
        Acme::Tiers => {
            project["tiers"] = serde_json::json!({ "capable": ["echo/echo"], "efficient": [] });
        }
        Acme::Learner(learner) => {
            project["tiers"] = serde_json::json!({ "capable": ["echo/echo"], "efficient": [] });
            project["learner"] = learner;
        }
    }
    let mut document = serde_json::json!({
        "projects": [project],
        "users": [{ "id": "ada" }],
        "keys": [{ "project": "acme", "user": "ada", "key_sha256": TURN_HASH }]
    });
    if recovery {
        document["learner_recovery"] = recovery_block();
    }
    document.to_string()
}

fn plane(document: &str) -> ControlPlane {
    ControlPlane::configured(
        ControlPlaneConfig::from_json(document, "test").expect("the document loads"),
    )
}

fn shadow_plane() -> ControlPlane {
    plane(&document(
        Acme::Learner(learner("shadow", valid_artifact())),
        true,
    ))
}

fn admission_of(plane: &ControlPlane) -> Admission {
    plane
        .configured_admissions()
        .next()
        .expect("one key")
        .clone()
}

async fn per_process() -> Backends {
    shared_backend::open(None, &KeyNamespace::default())
        .await
        .expect("memory backends open")
}

fn memory_store(backends: &Backends) -> Arc<MemoryStore> {
    match backends {
        Backends::PerProcess { store, .. } => Arc::clone(store),
        Backends::Shared { .. } => panic!("no Redis was named"),
    }
}

/// An engine over `store` wired the way `serve` wires it: the composed
/// policy, then the composed learner.
fn engine<S: SessionStore>(
    store: Arc<S>,
    composition: RoutingComposition,
) -> (
    Engine<S, ByteTokenizer>,
    Option<roundhouse_server::learner_recovery::LearnerRecovery<S>>,
) {
    let RoutingComposition { policy, learner } = composition;
    let engine = Engine::with_provider_clients(
        store,
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        catalog(),
        Arc::new(FrontierClients::uniform(Arc::new(EchoFrontierClient::new(
            "answered",
        )))),
        policy,
        EngineConfig::default(),
    );
    routing_composition::attach_learner(engine, learner)
}

async fn turn<S: SessionStore>(
    engine: &Engine<S, ByteTokenizer>,
    store: &S,
    session: &SessionId,
    turn: &str,
    admission: &Admission,
) -> DecisionRecord {
    engine.create_session(session).await.expect("a session");
    engine
        .run_turn(
            session,
            TurnId::new(turn),
            vec![Item::user_text(format!("question {turn}"))],
            admission,
        )
        .await
        .expect("the turn is served");
    SessionStore::read_events(store, session, 0, 10_000)
        .await
        .expect("the log reads")
        .into_iter()
        .rev()
        .find_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .expect("a routed turn")
}

fn is_learned(decision: &DecisionRecord) -> bool {
    matches!(
        decision
            .selection
            .as_ref()
            .and_then(|selection| selection.selector.as_ref())
            .map(|selector| &selector.branch),
        Some(SelectorBranch::Learned(_))
    )
}

/// **An M8-era boot with a `shadow` block now composes the learner store.**
/// Before M9 the binary attached none, so the block had no effect and gave no
/// warning. Now the composition holds a store, and a shadow turn's entries
/// land in it.
#[tokio::test]
async fn an_m8_era_boot_with_a_shadow_block_now_composes_the_learner_store() {
    let plane = shadow_plane();
    let backends = per_process().await;
    let composition = routing_composition::compose(&plane, &backends)
        .await
        .expect("the composition opens");
    let store = Arc::clone(
        &composition
            .learner
            .as_ref()
            .expect("a shadow project composes the learner")
            .store,
    );
    let sessions = memory_store(&backends);
    let (engine, _) = engine(Arc::clone(&sessions), composition);
    let admission = admission_of(&plane);
    let session = SessionId::new("acme/ada/m8");
    let decision = turn(&engine, &sessions, &session, "t1", &admission).await;
    assert!(
        is_learned(&decision),
        "the turn went through the learned seam"
    );
    assert!(
        store
            .watermark(&admission.principal.project, &session)
            .await
            .expect("the composed store answers")
            > 0,
        "the turn's entries landed in the composed store"
    );
}

/// **The whole composition**: the learned policy's name on the record, the
/// learner attached, and a recovery task over the same stores. The task is
/// shown to share them by making it confirm a delivery the engine already
/// made: it clears the requeued mark and sends nothing, which a task over
/// another learner store could not do.
#[tokio::test]
async fn a_learner_project_composes_the_learned_policy_and_recovery_task_at_boot() {
    let plane = shadow_plane();
    let backends = per_process().await;
    let composition = routing_composition::compose(&plane, &backends)
        .await
        .expect("the composition opens");
    assert_eq!(composition.policy.name(), "learned");
    assert!(
        composition.policy.reads_tier_recipes(),
        "the learned router reads the recipe, so no unread-recipe warning fires"
    );
    let store = memory_store(&backends);
    let (engine, recovery) = engine(Arc::clone(&store), composition);
    let mut recovery = recovery.expect("the recovery task is composed");
    let admission = admission_of(&plane);
    let session = SessionId::new("acme/ada/boot");
    let decision = turn(&engine, &store, &session, "t1", &admission).await;
    assert_eq!(decision.policy, "learned");
    assert!(is_learned(&decision));

    let marked = SessionStore::learning_sessions(store.as_ref(), None, NonZeroUsize::MIN)
        .await
        .expect("the index reads")
        .sessions
        .pop()
        .expect("the turn marked its session");
    SessionStore::requeue_learning(store.as_ref(), &session, marked.seq)
        .await
        .expect("the index answers");
    let config = MetricsConfig::new(catalog().shadow_pricing());
    let applied = |engine: &Engine<MemoryStore, ByteTokenizer>| {
        engine
            .metrics()
            .snapshot(&config, 0)
            .learning
            .delivery
            .expect("the deployment scope carries delivery")
            .applied_entries
    };
    let before = applied(&engine);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let report = recovery.sweep().await;
    assert_eq!(report.cleared, 1, "the task confirmed the requeued session");
    assert_eq!(
        applied(&engine),
        before,
        "and sent nothing: the store has it"
    );
}

/// **No project in `shadow` or `live`: exactly today's composition.** No
/// recipe is `affinity`, a recipe is `stage`, and an `off` learner block is
/// still `stage`. None of them composes a learner, opens a learner store, or
/// logs a learner line.
#[test]
fn no_learner_project_leaves_the_policy_name_unchanged() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    for (acme, recovery, expected) in [
        (Acme::Plain, false, "affinity"),
        (Acme::Tiers, false, "stage"),
        (
            Acme::Learner(serde_json::json!({ "mode": "off" })),
            false,
            "stage",
        ),
        // A file that prepared the recovery block for a learner the admin
        // plane may add later composes nothing either.
        (Acme::Tiers, true, "stage"),
    ] {
        let plane = plane(&document(acme, recovery));
        let warned = captured_warnings(|| {
            rt.block_on(async {
                let backends = per_process().await;
                let composition = routing_composition::compose(&plane, &backends)
                    .await
                    .expect("the composition opens");
                assert_eq!(composition.policy.name(), expected);
                assert!(composition.learner.is_none(), "{expected}: no learner");
                let store = memory_store(&backends);
                let (engine, recovery) = engine(Arc::clone(&store), composition);
                assert!(recovery.is_none(), "{expected}: no recovery task");
                let decision = turn(
                    &engine,
                    &store,
                    &SessionId::new("acme/ada/today"),
                    "t1",
                    &admission_of(&plane),
                )
                .await;
                assert_eq!(decision.policy, expected);
                assert!(!is_learned(&decision));
            });
        });
        assert!(
            !warned.contains("learner"),
            "{expected}: no learner line at boot or on a turn: {warned}"
        );
    }
}

/// **A learner block never goes quiet.** On the memory backend the store
/// warns that learner state is this process's and ends with it.
#[test]
fn the_memory_learner_store_warns_that_learner_state_ends_with_the_process() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let plane = shadow_plane();
    let warned = captured_warnings(|| {
        rt.block_on(async {
            let backends = per_process().await;
            routing_composition::compose(&plane, &backends)
                .await
                .expect("the composition opens");
        });
    });
    assert!(
        warned.contains("learner state is in this process's memory and ends with the process"),
        "{warned}"
    );
}

/// **An admin-added learner warns once per project until restart.** The
/// process booted with no learner, so a `shadow` block added later reaches an
/// engine that cannot run it: the turn routes as before, and each such
/// project is named once.
#[test]
fn an_admin_added_learner_warns_until_restart() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let booted = plane(&document(Acme::Tiers, true));
    let (store, (engine, recovery)) = rt.block_on(async {
        let backends = per_process().await;
        let composition = routing_composition::compose(&booted, &backends)
            .await
            .expect("the composition opens");
        let store = memory_store(&backends);
        (Arc::clone(&store), engine(store, composition))
    });
    assert!(recovery.is_none());
    // What the admin plane compiled after boot: the same project with a
    // `shadow` block, and a second project with one too.
    let later = shadow_plane();
    let acme = admission_of(&later);
    let mut beta = acme.clone();
    beta.principal = roundhouse_core::control::Principal::new("beta", "ada");

    let warned = captured_warnings(|| {
        rt.block_on(async {
            for (session, name) in [
                ("acme/ada/a", "t1"),
                ("acme/ada/a", "t2"),
                ("acme/ada/b", "t1"),
            ] {
                let decision = turn(&engine, &store, &SessionId::new(session), name, &acme).await;
                assert_eq!(decision.policy, "stage", "the turn routes as before");
                assert!(!is_learned(&decision));
            }
            turn(&engine, &store, &SessionId::new("beta/ada/a"), "t1", &beta).await;
        });
    });
    let lines: Vec<&str> = warned
        .lines()
        .filter(|line| line.contains("enables the learner, but this process composed none"))
        .collect();
    assert_eq!(lines.len(), 2, "one line per project: {warned}");
    assert!(lines[0].contains("acme"), "{}", lines[0]);
    assert!(lines[1].contains("beta"), "{}", lines[1]);
}

/// **An invalid artifact stops the boot.** The loader reads each artifact
/// when it validates the file, so the refusal is the file load's, before
/// anything is composed.
#[test]
fn an_invalid_artifact_stops_the_boot() {
    let broken = artifact_file(r#"{"schema_revision":1}"#);
    let file = artifact_file(&document(Acme::Learner(learner("shadow", broken)), true));
    match ControlPlaneConfig::load_fingerprinted(&file) {
        Err(ControlPlaneError::LearnerRejected {
            source: LearnerConfigError::Artifact { .. },
            ..
        }) => {}
        other => panic!("expected an artifact refusal, got {:?}", other.map(|_| ())),
    }
    // Control: the same file over a valid artifact loads.
    let valid = artifact_file(&document(
        Acme::Learner(learner("shadow", valid_artifact())),
        true,
    ));
    ControlPlaneConfig::load_fingerprinted(&valid).expect("a valid artifact loads");
}

/// The Redis arm: naming a Redis composes the Redis learner store, under the
/// deployment's namespace, and a second handle on that namespace sees what a
/// shadow turn applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_shadow_boot_on_redis_composes_the_shared_learner_store() {
    let url = std::env::var(roundhouse_store_redis::test_support::URL_VAR)
        .expect("ROUNDHOUSE_TEST_REDIS_URL names the test Redis");
    let namespace = KeyNamespace::new(format!("m9boot{}", uuid::Uuid::new_v4().simple()))
        .expect("a fresh namespace");
    let backends = shared_backend::open(Some(&url), &namespace)
        .await
        .expect("the Redis backends open");
    let plane = shadow_plane();
    let composition = routing_composition::compose(&plane, &backends)
        .await
        .expect("the composition opens");
    assert_eq!(composition.policy.name(), "learned");
    let store = match &backends {
        Backends::Shared { store, .. } => Arc::clone(store),
        Backends::PerProcess { .. } => panic!("naming a Redis must select the shared arm"),
    };
    let (engine, recovery) = engine(Arc::clone(&store), composition);
    assert!(recovery.is_some());
    let admission = admission_of(&plane);
    let session = SessionId::new("acme/ada/redis-boot");
    let decision = turn(&engine, &store, &session, "t1", &admission).await;
    assert!(is_learned(&decision));

    let other = roundhouse_store_redis::RedisLearnerStore::connect_namespaced(&url, namespace)
        .await
        .expect("a second handle");
    assert!(
        other
            .watermark(&admission.principal.project, &session)
            .await
            .expect("the Redis store answers")
            > 0,
        "the composed store is the shared one"
    );
}
