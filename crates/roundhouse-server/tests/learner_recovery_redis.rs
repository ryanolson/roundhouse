// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learner's reopen, successor and recovery cases against
//! `RedisSessionStore` and `RedisLearnerStore` (milestone M9, Redis-gated).
//!
//! Every test runs in a fresh namespace for both stores: the recovery task
//! and the audit enumerate the whole namespace's learning index, so a shared
//! namespace would sweep other suites' marks into this test's learner store.
//! Gated like the store's own integration tests: `#[ignore]`, opted into with
//! `--include-ignored`, and a missing `ROUNDHOUSE_TEST_REDIS_URL` then fails
//! loudly.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{Principal, ProjectId};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::learn_store::contract::LearnerStoreControl;
use roundhouse_core::learn_store::{
    Applied, LearnerError, LearnerStore, LearningBatch, ReadRequest,
};
use roundhouse_core::routing::learn::{
    EpochId, LearnedInput, LearnerMode, LearnerTerms, OnInfeasible, PriorUnits, QualityTerms,
    ReadView, Strategy, StrategySet,
};
use roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD;
use roundhouse_core::routing::{AffinityPolicy, PickerMode, StagePolicy, Target, Tier, TierRecipe};
use roundhouse_core::store::SessionStore;
use roundhouse_fleet::{EchoFrontierClient, FrontierClients, WireProtocol};
use roundhouse_server::learner_recovery::RecoveryCadence;
use roundhouse_server::test_support::{frontier_spec, single_model_catalog};
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, EngineConfig};
use roundhouse_store_redis::test_support::{fresh_namespace, url_from_env};
use roundhouse_store_redis::{KeyNamespace, RedisLearnerStore, RedisSessionStore};

/// The Redis learner store, whose next `refusals` applies do not land.
struct Refusing {
    inner: RedisLearnerStore,
    refusals: AtomicUsize,
}

#[async_trait]
impl LearnerStore for Refusing {
    async fn read(&self, request: &ReadRequest) -> Result<ReadView, LearnerError> {
        self.inner.read(request).await
    }

    async fn apply(&self, batch: &LearningBatch<'_>) -> Result<Applied, LearnerError> {
        if self
            .refusals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(LearnerError::Unavailable("down".into()));
        }
        self.inner.apply(batch).await
    }

    async fn watermark(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<u64, LearnerError> {
        self.inner.watermark(project, session).await
    }
}

fn echo() -> Target {
    Target::Frontier {
        provider: "echo".into(),
        model: "echo".into(),
    }
}

fn epoch() -> EpochId {
    EpochId::new([0x9e; 16])
}

fn terms() -> LearnerTerms {
    LearnerTerms {
        mode: LearnerMode::Shadow,
        strategies: StrategySet::new(vec![Strategy::Rules, Strategy::Capable]).unwrap(),
        epoch: epoch(),
        prior: PriorUnits::default(),
        quality: QualityTerms {
            floor: 0.8,
            z: 1.96,
            min_evidence: 5_000,
            min_sessions: 20,
        },
        latency_limit_ms: 10_000,
        latency_min_samples: 20,
        cache_min_samples: 20,
        on_infeasible: OnInfeasible::ServeRules,
        exploration: None,
        read_timeout_ms: 2_000,
    }
}

fn admission(project: &ProjectId) -> Admission {
    Admission {
        principal: Principal::new(project.as_str(), "ada"),
        tiers: Some(Arc::new(
            TierRecipe::new(
                vec!["echo/echo".into()],
                Vec::new(),
                PickerMode::CapableFirst,
                DEFAULT_CONFIDENCE_THRESHOLD,
            )
            .expect("a one-tier recipe"),
        )),
        learner: Some(Arc::new(terms())),
        learner_apply_timeout_ms: Some(2_000),
        ..Admission::open()
    }
}

fn cadence() -> RecoveryCadence {
    RecoveryCadence {
        sweep_interval: Duration::from_millis(30_000),
        idle_after_ms: 1,
        max_sessions_per_sweep: NonZeroUsize::new(64).unwrap(),
        pages_per_session_per_sweep: NonZeroUsize::new(4).unwrap(),
        audit_sessions_per_sweep: NonZeroUsize::new(32).unwrap(),
        read_timeout: Duration::from_millis(2_000),
        apply_timeout: Duration::from_millis(2_000),
        source_timeout: Duration::from_millis(2_000),
    }
}

/// One node: its own connections to both stores, under `namespace`.
struct Node {
    engine: Engine<RedisSessionStore, ByteTokenizer>,
    sessions: Arc<RedisSessionStore>,
    learner: Arc<Refusing>,
}

async fn node(namespace: &KeyNamespace, name: &str, refusals: usize) -> Node {
    let url = url_from_env();
    let sessions = Arc::new(
        RedisSessionStore::connect_namespaced(&url, namespace.clone())
            .await
            .expect("the session store connects"),
    );
    let learner = Arc::new(Refusing {
        inner: RedisLearnerStore::connect_namespaced(&url, namespace.clone())
            .await
            .expect("the learner store connects"),
        refusals: AtomicUsize::new(refusals),
    });
    let engine = Engine::with_provider_clients(
        Arc::clone(&sessions),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        single_model_catalog(frontier_spec("echo", "echo", WireProtocol::OpenAiResponses)),
        Arc::new(FrontierClients::uniform(Arc::new(EchoFrontierClient::new(
            "answered",
        )))),
        Arc::new(StagePolicy::new(Box::new(AffinityPolicy::new()))),
        EngineConfig {
            node_id: name.into(),
            ..EngineConfig::default()
        },
    )
    .with_learner(Arc::clone(&learner) as Arc<dyn LearnerStore>);
    Node {
        engine,
        sessions,
        learner,
    }
}

impl Node {
    async fn turn(&self, session: &SessionId, turn: &str, admission: &Admission) {
        self.engine
            .create_session(session)
            .await
            .expect("a session");
        self.engine
            .run_turn(
                session,
                TurnId::new(turn),
                vec![Item::user_text(format!("question {turn}"))],
                admission,
            )
            .await
            .expect("the turn is served");
    }

    /// `echo`'s residual samples in `project`: one per turn entry applied.
    async fn residuals(&self, project: &ProjectId) -> u64 {
        let request = ReadRequest::new(
            project.clone(),
            epoch(),
            &LearnedInput::encode(Tier::Capable, false, None, &[]),
            &terms().strategies,
            [&echo()],
        );
        let view = self.learner.inner.read(&request).await.expect("a read");
        view.target(&echo()).map_or(0, |ops| ops.latency.n)
    }

    async fn pending(&self) -> usize {
        SessionStore::pending_learning(
            self.sessions.as_ref(),
            None,
            0,
            NonZeroUsize::new(1_000).unwrap(),
        )
        .await
        .expect("the index reads")
        .sessions
        .len()
    }
}

fn fresh_project() -> ProjectId {
    ProjectId::new(format!("proj_{}", uuid::Uuid::new_v4().simple()))
}

fn idle() {
    std::thread::sleep(Duration::from_millis(5));
}

/// A session every one of whose applies failed is found in Redis's index and
/// delivered by the task, which appends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn the_recovery_task_delivers_a_session_every_apply_of_which_failed_on_redis() {
    let namespace = fresh_namespace();
    let project = fresh_project();
    let admission = admission(&project);
    let first = node(&namespace, "node-a", 2).await;
    let session = SessionId::new(format!("{project}/ada/s"));
    first.turn(&session, "t1", &admission).await;
    first.turn(&session, "t2", &admission).await;
    assert_eq!(first.residuals(&project).await, 0);
    assert_eq!(first.pending().await, 1);
    let events = SessionStore::last_seq(first.sessions.as_ref(), &session)
        .await
        .expect("a log");

    idle();
    let report = first
        .engine
        .learner_recovery(cadence())
        .expect("a learner")
        .sweep()
        .await;
    assert_eq!(report.cleared, 1);
    assert_eq!(first.residuals(&project).await, 2, "both turns, once each");
    assert_eq!(first.pending().await, 0);
    assert_eq!(
        SessionStore::last_seq(first.sessions.as_ref(), &session)
            .await
            .expect("a log"),
        events,
        "the task appended nothing"
    );
}

/// A successor node opens the session from Redis, replays it, and delivers
/// only its own new entries: the store skips what the first node applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_successor_reopens_the_session_and_applies_only_its_new_entries_on_redis() {
    let namespace = fresh_namespace();
    let project = fresh_project();
    let admission = admission(&project);
    let session = SessionId::new(format!("{project}/ada/s"));
    {
        let first = node(&namespace, "node-a", 0).await;
        first.turn(&session, "t1", &admission).await;
        assert_eq!(first.residuals(&project).await, 1);
    }
    let successor = node(&namespace, "node-b", 0).await;
    successor.turn(&session, "t2", &admission).await;
    successor.turn(&session, "t3", &admission).await;
    assert_eq!(successor.residuals(&project).await, 3, "each entry once");
    assert_eq!(successor.pending().await, 0);
}

/// Two nodes' tasks sweep one Redis session at once, and the audit restores a
/// project the learner store lost after its mark was cleared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn two_tasks_and_the_audit_keep_redis_counts_exact() {
    let namespace = fresh_namespace();
    let project = fresh_project();
    let admission = admission(&project);
    let session = SessionId::new(format!("{project}/ada/s"));
    let first = node(&namespace, "node-a", 2).await;
    let second = node(&namespace, "node-b", 0).await;
    let before = first.learner.inner.snapshot(&project).await;
    first.turn(&session, "t1", &admission).await;
    first.turn(&session, "t2", &admission).await;

    idle();
    let (mut one, mut two) = (
        first.engine.learner_recovery(cadence()).expect("a learner"),
        second
            .engine
            .learner_recovery(cadence())
            .expect("a learner"),
    );
    tokio::join!(one.sweep(), two.sweep());
    assert_eq!(first.residuals(&project).await, 2, "each entry once");
    assert_eq!(first.pending().await, 0);

    // The learner store loses the project after the clear.
    first.learner.inner.restore(&project, before).await;
    assert_eq!(first.residuals(&project).await, 0);
    let audited = one.sweep().await;
    assert_eq!(audited.requeued, 1, "the audit found the loss");
    assert_eq!(first.pending().await, 1);
    let restored = one.sweep().await;
    assert_eq!(restored.cleared, 1);
    assert_eq!(first.residuals(&project).await, 2, "restored from the log");
}
