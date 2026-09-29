// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The offline calibrator against real stores (milestone M10): enumeration
//! from the source marks, unreadable members, the read-only rule, the drift
//! check, the pinned cutoff, rollback, and the dump source.

mod learning_support;
mod offline_support;

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use learning_support::*;
use offline_support::seed;
use roundhouse_core::control::{Principal, ProjectId};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::{
    Applied, LearnerError, LearnerStore, LearningBatch, MemoryLearnerStore, ReadRequest,
};
use roundhouse_core::routing::learn::offline::{
    ArtifactPrior, BootstrapPlan, CalibrationConfig, CalibrationError, DriftCheck, DumpStore,
    LogDump, QualityMinimum, calibrate, log_digest, read_source,
};
use roundhouse_core::routing::learn::{Artifact, ReadView, Strategy, StrategySet, Units};
use roundhouse_core::session::{Deltas, LearningEntry, QualityDelta, SessionState};
use roundhouse_core::store::doubles::Delegating;
use roundhouse_core::store::{
    ClearOutcome, LearningCursor, LearningMark, LearningPage, Lease, MemoryStore, RequeueOutcome,
    SessionStore, StoreError,
};
use roundhouse_core::validate::Arm;
use std::num::NonZeroUsize;

fn config() -> CalibrationConfig {
    CalibrationConfig {
        project: ProjectId::new("acme"),
        strategies: StrategySet::new(vec![
            Strategy::Rules,
            Strategy::Efficient,
            Strategy::Capable,
        ])
        .unwrap(),
        prior: ArtifactPrior::Credit,
        quality: QualityMinimum { min_sessions: 20 },
        latency_limit_ms: 10_000,
        bootstrap: BootstrapPlan {
            seed: 3,
            resamples: 100,
        },
        cutoff: None,
    }
}

/// A shadow session of `acme/ada` with one reviewed turn per label.
fn learned(id: &str, labels: &[bool]) -> Script {
    let mut script = Script::named(id);
    for positive in labels {
        let turn = script.turn(Spec::new().decision());
        script.review(&[&turn], if *positive { on_track() } else { off_track() });
    }
    script
}

async fn store_with(scripts: &[&Script]) -> MemoryStore {
    let store = MemoryStore::new();
    for script in scripts {
        seed(&store, script).await;
    }
    store
}

async fn events(store: &MemoryStore, session: &SessionId) -> Vec<SessionEvent> {
    store.read_events(session, 0, 10_000).await.unwrap()
}

#[tokio::test]
async fn learner_sessions_are_enumerated_from_the_source_marks() {
    let marked = learned("acme/ada/marked#g0", &[true]);
    let mut unlearned = Script::named("acme/ada/plain#g0");
    let turn = unlearned.turn(learning_support::unlearned(opus()));
    unlearned.review(&[&turn], on_track());
    let mut other = Script::named_with(
        "globex/bob/other#g0",
        Some(Principal::new("globex", "bob")),
        Some(Arm::Shadow),
    );
    let turn = other.turn(Spec::new().decision());
    other.review(&[&turn], on_track());
    let store = store_with(&[&marked, &unlearned, &other]).await;

    let source = read_source(&store, &ProjectId::new("acme"), None)
        .await
        .unwrap();
    let read: Vec<&SessionId> = source.logs.iter().map(|log| &log.session).collect();
    assert_eq!(
        read,
        vec![&marked.session],
        "only the marked session of acme"
    );
    assert_eq!(source.census.project_sessions, 1);
    assert_eq!(source.census.other_projects, 1);
    let logged = events(&store, &marked.session).await;
    assert_eq!(source.input.sessions.len(), 1);
    assert_eq!(source.input.sessions[0].through_seq, logged.len() as u64);
    assert_eq!(source.input.sessions[0].sha256, log_digest(&logged));
}

#[tokio::test]
async fn unreadable_index_members_are_counted_as_an_exclusion_cause() {
    let good = learned("acme/ada/good#g0", &[true]);
    let bad = learned("acme/ada/bad#g0", &[true]);
    let store = store_with(&[&good, &bad]).await;
    store.make_learning_mark_unreadable(&bad.session).await;

    let calibrated = calibrate(&config(), &store, None, "c").await.unwrap();
    assert_eq!(
        calibrated.report.census.unreadable,
        vec![bad.session.clone()]
    );
    assert_eq!(calibrated.input.sessions.len(), 1);
    let text = calibrated.report.render();
    let line = text
        .lines()
        .find(|line| line.contains("unreadable index mark"))
        .unwrap();
    assert_eq!(
        line,
        "excluded, unreadable index mark: 1 sessions (sequence key) (acme/ada/bad#g0)"
    );
}

/// A session store double that counts every call that writes, leases, or
/// touches the pending set.
struct Watched {
    inner: MemoryStore,
    writes: AtomicUsize,
}

impl Watched {
    fn count(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Delegating for Watched {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn create_session(&self, id: &SessionId, policy: &str) -> Result<bool, StoreError> {
        self.count();
        self.inner.create_session(id, policy).await
    }

    async fn acquire_lease(
        &self,
        id: &SessionId,
        node: &str,
        ttl: u64,
    ) -> Result<Option<Lease>, StoreError> {
        self.count();
        self.inner.acquire_lease(id, node, ttl).await
    }

    async fn renew_lease(&self, lease: &Lease, ttl: u64) -> Result<Option<Lease>, StoreError> {
        self.count();
        self.inner.renew_lease(lease, ttl).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.count();
        self.inner.release_lease(lease).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.count();
        self.inner.append_events(lease, kinds, mark).await
    }

    async fn clear_learning_mark(
        &self,
        id: &SessionId,
        through: u64,
    ) -> Result<ClearOutcome, StoreError> {
        self.count();
        self.inner.clear_learning_mark(id, through).await
    }

    async fn requeue_learning(
        &self,
        id: &SessionId,
        seq: u64,
    ) -> Result<RequeueOutcome, StoreError> {
        self.count();
        self.inner.requeue_learning(id, seq).await
    }

    async fn pending_learning(
        &self,
        after: Option<&LearningCursor>,
        idle: u64,
        limit: NonZeroUsize,
    ) -> Result<LearningPage, StoreError> {
        self.count();
        self.inner.pending_learning(after, idle, limit).await
    }
}

/// A learner store that counts applies.
struct WatchedLearner {
    inner: MemoryLearnerStore,
    applies: AtomicUsize,
    reads: AtomicUsize,
}

#[async_trait]
impl LearnerStore for WatchedLearner {
    async fn read(&self, request: &ReadRequest) -> Result<ReadView, LearnerError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read(request).await
    }

    async fn apply(&self, batch: &LearningBatch<'_>) -> Result<Applied, LearnerError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
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

/// Every session's whole entry chain, applied to `store` as its learner did.
async fn deliver(store: &dyn LearnerStore, source: &MemoryStore, scripts: &[&Script]) {
    let project = ProjectId::new("acme");
    for script in scripts {
        let log = events(source, &script.session).await;
        let entries = SessionState::replay_learning(&log).entries;
        store
            .apply(&LearningBatch {
                project: &project,
                session: &script.session,
                entries: &entries,
            })
            .await
            .expect("the chain applies");
    }
}

#[tokio::test]
async fn the_calibrator_is_read_only_against_both_stores() {
    let one = learned("acme/ada/one#g0", &[true, false]);
    let two = learned("acme/ada/two#g0", &[true]);
    let store = Watched {
        inner: store_with(&[&one, &two]).await,
        writes: AtomicUsize::new(0),
    };
    let copy = WatchedLearner {
        inner: MemoryLearnerStore::new(),
        applies: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
    };
    deliver(&copy.inner, &store.inner, &[&one, &two]).await;
    let before = (
        events(&store.inner, &one.session).await,
        events(&store.inner, &two.session).await,
    );

    let calibrated = calibrate(&config(), &store, Some(&copy as &dyn LearnerStore), "c")
        .await
        .unwrap();
    assert!(matches!(calibrated.report.drift, DriftCheck::Ran(_)));
    assert!(
        copy.reads.load(Ordering::SeqCst) > 0,
        "the drift check read the copy"
    );
    assert_eq!(
        store.writes.load(Ordering::SeqCst),
        0,
        "no write, lease or pending call"
    );
    assert_eq!(copy.applies.load(Ordering::SeqCst), 0, "no apply");
    let after = (
        events(&store.inner, &one.session).await,
        events(&store.inner, &two.session).await,
    );
    assert_eq!(before, after);
    let pending = store
        .inner
        .pending_learning(None, 0, NonZeroUsize::new(10).unwrap())
        .await
        .unwrap();
    assert_eq!(
        pending.sessions.len(),
        2,
        "both marks still pending: nothing cleared"
    );
}

#[tokio::test]
async fn the_drift_check_does_not_run_without_a_point_in_time_copy() {
    let one = learned("acme/ada/one#g0", &[true]);
    let store = store_with(&[&one]).await;
    let calibrated = calibrate(&config(), &store, None, "c").await.unwrap();
    assert_eq!(calibrated.report.drift, DriftCheck::NotRun);
    let text = calibrated.report.render();
    assert!(
        text.contains("drift check not run"),
        "the report says so:\n{text}"
    );
}

#[tokio::test]
async fn the_drift_check_compares_a_copy_with_the_entries_its_watermarks_cover() {
    let one = learned("acme/ada/one#g0", &[true, false]);
    let two = learned("acme/ada/two#g0", &[true]);
    let store = store_with(&[&one, &two]).await;
    let copy = MemoryLearnerStore::new();
    deliver(&copy, &store, &[&one, &two]).await;

    let calibrated = calibrate(&config(), &store, Some(&copy as &dyn LearnerStore), "c")
        .await
        .unwrap();
    let DriftCheck::Ran(clean) = &calibrated.report.drift else {
        panic!("a copy was given");
    };
    assert!(clean.compared > 0);
    assert!(clean.differences.is_empty(), "{:?}", clean.differences);

    // Credit the copy holds that no log in the manifest explains.
    let key = calibrated.evidence.prior.keys().next().unwrap().0;
    let epoch = learning_support::epoch();
    let ghost = SessionId::new("acme/ada/ghost#g0");
    let project = ProjectId::new("acme");
    let entries = [LearningEntry {
        seq: 1,
        prev_seq: 0,
        credit_revision: 1,
        review_rule_revision: 1,
        deltas: Some(Deltas {
            epoch,
            quality: vec![QualityDelta {
                key,
                strategy: Strategy::Rules,
                units: Units { pos: 7, n: 7 },
            }],
            targets: Vec::new(),
            overhead: Default::default(),
            jev: Vec::new(),
        }),
    }];
    copy.apply(&LearningBatch {
        project: &project,
        session: &ghost,
        entries: &entries,
    })
    .await
    .unwrap();
    let calibrated = calibrate(&config(), &store, Some(&copy as &dyn LearnerStore), "c")
        .await
        .unwrap();
    let DriftCheck::Ran(drifted) = &calibrated.report.drift else {
        panic!("a copy was given");
    };
    assert_eq!(drifted.differences.len(), 3, "{:?}", drifted.differences);
    assert!(
        drifted
            .differences
            .iter()
            .any(|difference| difference
                .ends_with(":rules:pos: copy holds 2007, rebuild gives 2000")),
        "{:?}",
        drifted.differences
    );
}

/// A copy that holds only part of a session holds what its watermark says:
/// the rebuild stops at the watermark, so a copy taken mid-delivery is not
/// drift.
#[tokio::test]
async fn the_drift_check_rebuilds_only_through_each_copy_watermark() {
    let one = learned("acme/ada/one#g0", &[true, false, true]);
    let store = store_with(&[&one]).await;
    let copy = MemoryLearnerStore::new();
    let log = events(&store, &one.session).await;
    let entries = SessionState::replay_learning(&log).entries;
    let half = &entries[..entries.len() / 2];
    copy.apply(&LearningBatch {
        project: &ProjectId::new("acme"),
        session: &one.session,
        entries: half,
    })
    .await
    .unwrap();
    let calibrated = calibrate(&config(), &store, Some(&copy as &dyn LearnerStore), "c")
        .await
        .unwrap();
    let DriftCheck::Ran(result) = &calibrated.report.drift else {
        panic!("a copy was given");
    };
    assert!(result.differences.is_empty(), "{:?}", result.differences);
}

#[tokio::test]
async fn a_pinned_log_that_no_longer_matches_its_digest_is_refused() {
    let one = learned("acme/ada/one#g0", &[true]);
    let store = store_with(&[&one]).await;
    let first = calibrate(&config(), &store, None, "c").await.unwrap();
    let mut pinned = first.input.sessions.clone();
    pinned[0].sha256 = "00".repeat(32);
    let config = CalibrationConfig {
        cutoff: Some(pinned),
        ..config()
    };
    assert!(matches!(
        calibrate(&config, &store, None, "c").await,
        Err(CalibrationError::Cutoff(_))
    ));
}

/// Rollback names the previous artifact again (draft 14.6). Its bytes give its
/// epoch, a rerun pinned to its manifest writes the same bytes, and the store
/// still holds the counters learned under that epoch.
#[tokio::test]
async fn rolling_back_the_artifact_reads_the_previous_epoch_state() {
    let one = learned("acme/ada/one#g0", &[true]);
    let store = store_with(&[&one]).await;
    let previous = calibrate(&config(), &store, None, "c").await.unwrap();
    let two = learned("acme/ada/two#g0", &[false]);
    seed(&store, &two).await;
    let next = calibrate(&config(), &store, None, "c").await.unwrap();
    assert_ne!(previous.parsed.epoch(), next.parsed.epoch());

    let pinned = CalibrationConfig {
        cutoff: Some(previous.input.sessions.clone()),
        ..config()
    };
    let rerun = calibrate(&pinned, &store, None, "c").await.unwrap();
    assert_eq!(
        rerun.artifact, previous.artifact,
        "the pinned manifest writes the same bytes"
    );
    assert_eq!(rerun.report.census.after_cutoff, vec![two.session.clone()]);

    let learner = MemoryLearnerStore::new();
    let project = ProjectId::new("acme");
    let key = previous.evidence.prior.keys().next().unwrap().0;
    for (epoch, pos, session) in [
        (previous.parsed.epoch(), 1_000, "acme/ada/x#g0"),
        (next.parsed.epoch(), 0, "acme/ada/y#g0"),
    ] {
        let entries = [LearningEntry {
            seq: 1,
            prev_seq: 0,
            credit_revision: 1,
            review_rule_revision: 1,
            deltas: Some(Deltas {
                epoch,
                quality: vec![QualityDelta {
                    key,
                    strategy: Strategy::Rules,
                    units: Units { pos, n: 1_000 },
                }],
                targets: Vec::new(),
                overhead: Default::default(),
                jev: Vec::new(),
            }),
        }];
        learner
            .apply(&LearningBatch {
                project: &project,
                session: &SessionId::new(session),
                entries: &entries,
            })
            .await
            .unwrap();
    }
    let rolled_back = Artifact::parse(&previous.artifact).unwrap();
    assert_eq!(rolled_back.epoch(), previous.parsed.epoch());
    let input = learning_support::input(
        roundhouse_core::routing::Tier::Capable,
        roundhouse_core::routing::learn::Band::None,
    );
    let read = |epoch| {
        ReadRequest::new(
            project.clone(),
            epoch,
            &input,
            rolled_back.strategies(),
            std::iter::empty(),
        )
    };
    let view = learner.read(&read(rolled_back.epoch())).await.unwrap();
    let rules = |view: &ReadView| {
        view.levels
            .iter()
            .find(|level| level.key == key)
            .and_then(|level| {
                level
                    .strategies
                    .iter()
                    .find(|s| s.strategy == Strategy::Rules)
            })
            .map(|counts| (counts.pos_units, counts.n_units))
    };
    assert_eq!(
        rules(&view),
        Some((1_000, 1_000)),
        "the previous epoch's state"
    );
    let view = learner.read(&read(next.parsed.epoch())).await.unwrap();
    assert_eq!(rules(&view), Some((0, 1_000)));
}

/// A dump captured from a store serves the same logs and marks, so a fixture
/// run over it reports exactly what a run over the store reports.
#[tokio::test]
async fn a_dump_reports_what_the_store_it_was_captured_from_reports() {
    let one = learned("acme/ada/one#g0", &[true, false]);
    let two = learned("acme/ada/two#g0", &[true]);
    let store = store_with(&[&one, &two]).await;
    let dump = LogDump::capture(&store).await.unwrap();
    let bytes = serde_json::to_vec(&dump).unwrap();
    let restored: LogDump = serde_json::from_slice(&bytes).unwrap();
    let fixture = DumpStore::new(restored).unwrap();

    let from_store = calibrate(&config(), &store, None, "c").await.unwrap();
    let from_dump = calibrate(&config(), &fixture, None, "c").await.unwrap();
    assert_eq!(from_store.artifact, from_dump.artifact);
    assert_eq!(from_store.report.render(), from_dump.report.render());
    assert!(matches!(
        fixture.create_session(&one.session, "stage").await,
        Err(StoreError::Backend(_))
    ));
}
