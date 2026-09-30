// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The recovery task: idle sessions with
//! entries owed are found in the session store's index and delivered without
//! the engine, through the same delivery the tail runs.
//!
//! Every sweep here waits a few milliseconds of wall clock first, because the
//! memory store stamps marks with the wall clock and a sweep takes only the
//! marks at least `idle_after_ms` old.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::LearnerError;
use roundhouse_core::learn_store::contract::LearnerStoreControl;
use roundhouse_core::routing::learn::{LearnerMode, LearnerTerms};
use roundhouse_core::session::LEARNING_PAGE;
use roundhouse_core::store::{ClearOutcome, SessionStore};
use roundhouse_server::learner_recovery::{LearnerRecovery, RecoveryCadence, backoff};
use roundhouse_server::test_support::captured_warnings;

use crate::rig::{
    ApplyScript, CountingStore, Rig, RigConfig, admission, delivery, fresh_input, large, project,
    residuals, terms,
};

pub(crate) fn shadow() -> LearnerTerms {
    terms(LearnerMode::Shadow)
}

/// The section 4 cadence, with a 1 ms idle window so a test waits only a few
/// milliseconds for its marks to count as idle.
pub(crate) fn cadence() -> RecoveryCadence {
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

pub(crate) fn recovery(rig: &Rig) -> LearnerRecovery<CountingStore> {
    rig.engine
        .learner_recovery(cadence())
        .expect("the rig attaches a learner")
}

/// Wait past the idle window by the wall clock the memory store stamps with.
pub(crate) async fn idle() {
    std::thread::sleep(Duration::from_millis(5));
}

/// The next `n` tail applies do not land: the store is down for them.
pub(crate) fn refuse_applies(rig: &Rig, n: usize) {
    for _ in 0..n {
        rig.learner
            .script(ApplyScript::Refuse(LearnerError::Unavailable(
                "down".into(),
            )));
    }
}

/// Every pending session, idle or not.
pub(crate) async fn pending(rig: &Rig) -> Vec<SessionId> {
    SessionStore::pending_learning(
        rig.sessions.as_ref(),
        None,
        0,
        NonZeroUsize::new(1_000).unwrap(),
    )
    .await
    .expect("the index reads")
    .sessions
    .into_iter()
    .map(|marked| marked.session_id)
    .collect()
}

/// **Failure before the first apply**: the append of the first
/// entry-producing event marked the session, the node died before any apply
/// landed, and a successor's task finds the session once it is idle.
#[tokio::test]
async fn a_crash_before_the_first_apply_is_recovered_by_the_recovery_task() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("crash/ada/s");
    let admission = admission("crash", Some(shadow()));
    refuse_applies(&rig, 1);
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(residuals(&rig, "crash").await, 0);
    assert_eq!(pending(&rig).await, vec![session.clone()]);

    // The node is gone; a successor over the same stores runs the task.
    let successor = Rig::over(
        RigConfig::default(),
        Arc::clone(&rig.sessions),
        Arc::clone(&rig.learner),
    );
    drop(rig.engine);
    idle().await;
    let report = recovery(&successor).sweep().await;
    assert_eq!(residuals(&successor, "crash").await, 1, "delivered once");
    assert_eq!(report.cleared, 1);
    assert!(pending(&successor).await.is_empty(), "the mark is cleared");
    assert_eq!(
        successor.sessions.acks(),
        0,
        "the task appended no acknowledgement"
    );
}

/// **Every learner-store call of a session failed**: the reads
/// and the applies. The source marks keep the session, and the task delivers
/// it once the store answers again.
#[tokio::test]
async fn a_session_whose_every_learner_store_call_failed_is_delivered_after_recovery() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("dark/ada/s");
    let admission = admission("dark", Some(shadow()));
    rig.learner.fail_reads(true);
    refuse_applies(&rig, 2);
    rig.turn(&session, "t1", &admission).await.expect("served");
    rig.turn(&session, "t2", &admission).await.expect("served");
    assert_eq!(residuals(&rig, "dark").await, 0);

    rig.learner.fail_reads(false);
    idle().await;
    recovery(&rig).sweep().await;
    assert_eq!(residuals(&rig, "dark").await, 2, "both turns, once each");
    assert!(pending(&rig).await.is_empty());
}

/// **Done means**: a session delivered only by the task reaches the same
/// counters as one the engine's tail delivered.
#[tokio::test]
async fn a_recovered_session_reaches_the_same_counters_as_an_engine_delivered_one() {
    let rig = Rig::new(RigConfig::default());
    let by_engine = admission("byengine", Some(shadow()));
    let by_task = admission("bytask", Some(shadow()));
    for turn in ["t1", "t2"] {
        rig.turn(&SessionId::new("byengine/ada/s"), turn, &by_engine)
            .await
            .expect("served");
    }
    let engine_delivery = delivery(&rig);
    refuse_applies(&rig, 2);
    for turn in ["t1", "t2"] {
        rig.turn(&SessionId::new("bytask/ada/s"), turn, &by_task)
            .await
            .expect("served");
    }
    idle().await;
    recovery(&rig).sweep().await;

    let view = |project_name: &'static str| {
        let rig = &rig;
        async move {
            let request = roundhouse_core::learn_store::ReadRequest::new(
                project(project_name),
                crate::rig::epoch(),
                &fresh_input(),
                &shadow().strategies,
                [&large(), &crate::rig::small()],
            );
            let view = rig
                .learner
                .inner
                .read(&request)
                .await
                .expect("a memory read");
            let ops = view.target(&large()).expect("large is read").clone();
            (ops.latency.n, view.overhead.n, view.levels.len())
        }
    };
    use roundhouse_core::learn_store::LearnerStore;
    assert_eq!(view("bytask").await, view("byengine").await);
    let total = delivery(&rig);
    assert_eq!(
        total.applied_entries,
        2 * engine_delivery.applied_entries,
        "the task's applies count where the tail's do"
    );
    assert_eq!(total.duplicate_entries, 0);
}

/// **A delayed clear cannot remove a newer mark**. Node A
/// delivers turn 1 and its clear is delayed past a lease turnover; the
/// successor's turn 2 marks the session and does not deliver. A's clear
/// arrives with turn 1's watermark and leaves turn 2's mark pending, and the
/// task delivers it.
#[tokio::test]
async fn a_delayed_engine_clear_after_lease_turnover_keeps_the_new_turn_mark() {
    let first = Rig::new(RigConfig::default());
    let session = SessionId::new("turnover/ada/s");
    let admission = admission("turnover", Some(shadow()));
    first
        .turn(&session, "t1", &admission)
        .await
        .expect("served");
    let confirmed = *first
        .acknowledged(&session)
        .await
        .last()
        .expect("turn 1 was delivered");

    let successor = Rig::over(
        RigConfig::default(),
        Arc::clone(&first.sessions),
        Arc::clone(&first.learner),
    );
    refuse_applies(&successor, 1);
    successor
        .turn(&session, "t2", &admission)
        .await
        .expect("served");

    // A's delayed clear, with the watermark it confirmed for turn 1.
    let cleared = SessionStore::clear_learning_mark(first.sessions.as_ref(), &session, confirmed)
        .await
        .expect("the index answers");
    assert!(
        matches!(cleared, ClearOutcome::Newer { mark_seq } if mark_seq > confirmed),
        "{cleared:?}"
    );
    assert_eq!(pending(&successor).await, vec![session.clone()]);

    idle().await;
    recovery(&successor).sweep().await;
    assert_eq!(residuals(&successor, "turnover").await, 2);
    assert!(pending(&successor).await.is_empty());
}

/// **A lost acknowledgement**: the store applied, and the
/// `LearningApplied` append failed, so the mark stays. The task finds nothing
/// above the store's watermark to send, and the clear with that watermark
/// covers the mark.
#[tokio::test]
async fn a_lost_ack_leaves_the_mark_and_the_recovery_task_clears_it_once_delivered() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("lostack/ada/s");
    let admission = admission("lostack", Some(shadow()));
    rig.sessions.refuse_acks(true);
    rig.turn(&session, "t1", &admission).await.expect("served");
    rig.sessions.refuse_acks(false);
    assert_eq!(residuals(&rig, "lostack").await, 1, "the apply landed");
    assert_eq!(
        pending(&rig).await,
        vec![session.clone()],
        "the mark stayed"
    );

    let applies = rig.learner.applies();
    idle().await;
    let report = recovery(&rig).sweep().await;
    assert_eq!(report.cleared, 1);
    assert!(pending(&rig).await.is_empty(), "the clear covered the mark");
    assert_eq!(
        rig.learner.applies(),
        applies,
        "nothing above the watermark, so nothing was resent"
    );
    assert_eq!(residuals(&rig, "lostack").await, 1, "and nothing twice");
}

/// **The task never appends**, and a sweep that races the owner's own turn on
/// the same session applies each entry once: the entry identity rule makes
/// the apply safe and the clear predicate keeps any newer mark.
#[tokio::test]
async fn the_recovery_task_never_appends_and_is_safe_while_the_owner_runs() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("owner/ada/s");
    let admission = admission("owner", Some(shadow()));
    refuse_applies(&rig, 2);
    rig.turn(&session, "t1", &admission).await.expect("served");
    rig.turn(&session, "t2", &admission).await.expect("served");
    let events = rig.events(&session).await.len();
    let acks = rig.sessions.acks();

    idle().await;
    let mut task = recovery(&rig);
    task.sweep().await;
    assert_eq!(residuals(&rig, "owner").await, 2);
    assert_eq!(
        rig.events(&session).await.len(),
        events,
        "a sweep writes nothing to the log"
    );
    assert_eq!(rig.sessions.acks(), acks, "and appends no acknowledgement");

    // Two more owed entries, then a sweep racing the owner's next turn.
    refuse_applies(&rig, 2);
    rig.turn(&session, "t3", &admission).await.expect("served");
    rig.turn(&session, "t4", &admission).await.expect("served");
    idle().await;
    let (served, _) = tokio::join!(rig.turn(&session, "t5", &admission), task.sweep());
    served.expect("served");
    // Whichever side lost the race, a later sweep finishes.
    idle().await;
    task.sweep().await;
    assert_eq!(residuals(&rig, "owner").await, 5, "every entry, once each");
    assert!(pending(&rig).await.is_empty());
}

/// **The task does not ask about the lease.** `CountingStore` is a
/// `Delegating` double, which inherits `SessionStore::is_leased`'s default of
/// `true` for every session. A task that skipped leased sessions would stall
/// on every such backend.
#[tokio::test]
async fn the_recovery_task_delivers_on_a_store_that_inherits_the_is_leased_default() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("leased/ada/s");
    let admission = admission("leased", Some(shadow()));
    refuse_applies(&rig, 1);
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert!(
        SessionStore::is_leased(rig.sessions.as_ref(), &session)
            .await
            .expect("answers"),
        "the double inherits the default and calls an idle session leased"
    );
    assert!(
        !SessionStore::is_leased(&rig.sessions.inner, &session)
            .await
            .expect("answers"),
        "control: the memory store itself knows the session is idle"
    );

    idle().await;
    recovery(&rig).sweep().await;
    assert_eq!(residuals(&rig, "leased").await, 1);
    assert!(pending(&rig).await.is_empty());
}

/// **Two tasks on two nodes** visit the same session at once. The entry
/// identity rule applies each entry once, and the clear predicate keeps the
/// index right.
#[tokio::test]
async fn two_recovery_tasks_on_one_session_apply_each_entry_once() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("twice/ada/s");
    let admission = admission("twice", Some(shadow()));
    refuse_applies(&rig, 2);
    rig.turn(&session, "t1", &admission).await.expect("served");
    rig.turn(&session, "t2", &admission).await.expect("served");
    let other = Rig::over(
        RigConfig::default(),
        Arc::clone(&rig.sessions),
        Arc::clone(&rig.learner),
    );

    idle().await;
    let (mut one, mut two) = (recovery(&rig), recovery(&other));
    tokio::join!(one.sweep(), two.sweep());
    assert_eq!(residuals(&rig, "twice").await, 2, "each entry once");
    assert!(pending(&rig).await.is_empty());
}

/// **The audit**: the learner store lost what an earlier clear
/// confirmed, so its watermark is below the permanent mark. The audit makes
/// the session pending again through `requeue_learning`, and the next sweep
/// restores the counters from the log. The audit never clears.
#[tokio::test]
async fn the_audit_requeues_a_session_whose_learner_watermark_fell_below_its_mark() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("audit/ada/s");
    let admission = admission("audit", Some(shadow()));
    let before = rig.learner.inner.snapshot(&project("audit")).await;
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(residuals(&rig, "audit").await, 1);
    assert!(pending(&rig).await.is_empty(), "the tail cleared the mark");

    let mut task = recovery(&rig);
    idle().await;
    let control = task.sweep().await;
    assert_eq!(
        control.requeued, 0,
        "control: a watermark at the mark is not requeued"
    );

    // The learner store loses the project.
    rig.learner.inner.restore(&project("audit"), before).await;
    assert_eq!(residuals(&rig, "audit").await, 0);
    let audited = task.sweep().await;
    assert_eq!(audited.requeued, 1, "the audit found the loss");
    assert_eq!(audited.cleared, 0, "and cleared nothing");
    assert_eq!(pending(&rig).await, vec![session.clone()]);

    let restored = task.sweep().await;
    assert_eq!(restored.cleared, 1);
    assert_eq!(residuals(&rig, "audit").await, 1, "restored from the log");
    assert!(pending(&rig).await.is_empty());
}

/// **The clear predicate**: with one page per session per sweep and more than
/// a page owed, the first sweep delivers a page and confirms a watermark below
/// the mark, so the mark stays pending. The second sweep delivers the rest
/// and the clear covers it. A task that cleared with the mark's own sequence,
/// or without the store's answer, would drop the session after one page.
///
/// The audit repairs exactly that loss, so it must not reach this session in
/// the first sweep or it would hide the defect: its page is one session, and
/// an earlier session in byte order (`a-first`) takes it.
#[tokio::test]
async fn the_clear_predicate_keeps_the_mark_until_the_last_page_is_delivered() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("pages/ada/s");
    let admission = admission("pages", Some(shadow()));
    rig.turn(&SessionId::new("a-first/ada/s"), "t1", &admission)
        .await
        .expect("served");
    let backlog = LEARNING_PAGE + 1;
    refuse_applies(&rig, backlog);
    for turn in 0..backlog {
        rig.turn(&session, &format!("b{turn}"), &admission)
            .await
            .expect("served");
    }
    let mut task = rig
        .engine
        .learner_recovery(RecoveryCadence {
            pages_per_session_per_sweep: NonZeroUsize::MIN,
            audit_sessions_per_sweep: NonZeroUsize::MIN,
            ..cadence()
        })
        .expect("a learner");

    idle().await;
    let first = task.sweep().await;
    assert_eq!(residuals(&rig, "pages").await, 1 + LEARNING_PAGE as u64);
    assert_eq!(first.cleared, 0);
    assert_eq!(
        pending(&rig).await,
        vec![session.clone()],
        "a page short of the mark leaves it pending"
    );

    let second = task.sweep().await;
    assert_eq!(residuals(&rig, "pages").await, 1 + backlog as u64);
    assert_eq!(second.cleared, 1);
    assert!(pending(&rig).await.is_empty());
}

/// The page budget is per session per sweep: with the section 4 budget of
/// four pages, a backlog one entry past a page is delivered, and its mark
/// cleared, in one sweep.
#[tokio::test]
async fn one_sweep_delivers_as_many_pages_as_its_budget_allows() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("budget/ada/s");
    let admission = admission("budget", Some(shadow()));
    let backlog = LEARNING_PAGE + 1;
    refuse_applies(&rig, backlog);
    for turn in 0..backlog {
        rig.turn(&session, &format!("b{turn}"), &admission)
            .await
            .expect("served");
    }
    idle().await;
    let report = recovery(&rig).sweep().await;
    assert_eq!(residuals(&rig, "budget").await, backlog as u64);
    assert_eq!(report.cleared, 1);
    assert!(pending(&rig).await.is_empty());
}

/// **A store outage** during a sweep: the task backs off and logs once per
/// outage, not once per session and not once per sweep, and delivers every
/// session once the store returns.
///
/// Synchronous with a current-thread runtime, because `captured_warnings`
/// captures only the calling thread's events.
#[test]
fn a_store_outage_during_a_recovery_sweep_logs_once_and_delivers_after_the_store_returns() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let rig = Rig::new(RigConfig::default());
    let admission = admission("outage", Some(shadow()));
    let sessions = [
        SessionId::new("outage/ada/a"),
        SessionId::new("outage/ada/b"),
    ];
    let mut task = rt.block_on(async {
        refuse_applies(&rig, sessions.len());
        for session in &sessions {
            rig.turn(session, "t1", &admission).await.expect("served");
        }
        recovery(&rig)
    });
    rig.learner.fail_watermarks(true);
    let interval = cadence().sweep_interval;

    let warned = captured_warnings(|| {
        rt.block_on(async {
            idle().await;
            for _ in 0..2 {
                let report = task.sweep().await;
                assert!(report.outage, "the sweep met the outage");
                assert_eq!(report.cleared, 0);
            }
        });
    });
    assert_eq!(
        warned
            .matches("the learner recovery task cannot reach a store")
            .count(),
        1,
        "two sweeps over two sessions, one line: {warned}"
    );
    assert!(
        task.next_delay() > interval,
        "the task backs off during an outage"
    );

    rig.learner.fail_watermarks(false);
    rt.block_on(async {
        let report = task.sweep().await;
        assert!(!report.outage);
        assert_eq!(report.cleared, sessions.len());
        assert_eq!(residuals(&rig, "outage").await, sessions.len() as u64);
        assert!(pending(&rig).await.is_empty());
    });
    assert_eq!(
        task.next_delay(),
        interval,
        "a clean sweep resets the backoff"
    );
}

/// **One bad session does not stall the task.** A project whose learner keys
/// hold foreign data answers `WrongType`: that is one session's problem, not
/// an outage. The task must hold that session and deliver the ones behind it
/// in byte order, sweep after sweep; ending the sweep with the cursor unmoved
/// would retry the same session first forever and starve every project.
#[tokio::test]
async fn a_wrong_typed_session_does_not_stall_the_sessions_behind_it() {
    let rig = Rig::new(RigConfig::default());
    // Both bad sessions sort before the good one: `bad` has a foreign
    // watermark key, `baf` a foreign counter key its apply meets.
    let bad = SessionId::new("bad/ada/s");
    let baf = SessionId::new("baf/ada/s");
    let good = SessionId::new("good/ada/s");
    refuse_applies(&rig, 3);
    for (session, project) in [(&bad, "bad"), (&baf, "baf"), (&good, "good")] {
        rig.turn(session, "t1", &admission(project, Some(shadow())))
            .await
            .expect("served");
    }
    rig.learner.wrong_type_for("bad");
    rig.learner.wrong_type_applies_for("baf");

    idle().await;
    let mut task = recovery(&rig);
    let first = task.sweep().await;
    assert!(!first.outage, "a foreign key is not an outage");
    assert_eq!(
        residuals(&rig, "good").await,
        1,
        "the session behind them is delivered"
    );
    assert_eq!(
        pending(&rig).await,
        vec![bad.clone(), baf.clone()],
        "the bad ones stay pending"
    );

    // New work behind the bad sessions, on a later sweep.
    refuse_applies(&rig, 1);
    rig.turn(&good, "t2", &admission("good", Some(shadow())))
        .await
        .expect("served");
    idle().await;
    let second = task.sweep().await;
    assert!(!second.outage);
    assert_eq!(residuals(&rig, "good").await, 2);
    assert_eq!(task.next_delay(), cadence().sweep_interval, "no backoff");
}

/// **A superseded generation** is a sequence like any other: a compaction
/// started `#g2`, and `#g1` still owes entries. The task delivers them and
/// clears its mark.
#[tokio::test]
async fn a_superseded_generation_is_delivered_and_cleared() {
    let rig = Rig::new(RigConfig::default());
    let admission = admission("gens", Some(shadow()));
    let old = SessionId::new("gens/ada/thread#g1");
    let new = SessionId::new("gens/ada/thread#g2");
    refuse_applies(&rig, 1);
    rig.turn(&old, "t1", &admission).await.expect("served");
    rig.turn(&new, "t1", &admission).await.expect("served");
    assert_eq!(pending(&rig).await, vec![old.clone()]);

    idle().await;
    recovery(&rig).sweep().await;
    assert_eq!(residuals(&rig, "gens").await, 2);
    assert!(pending(&rig).await.is_empty());
}

#[test]
fn the_backoff_doubles_per_outage_and_stops_at_eight_intervals() {
    let interval = Duration::from_millis(1_000);
    assert_eq!(backoff(interval, 0), interval);
    assert_eq!(backoff(interval, 1), interval * 2);
    assert_eq!(backoff(interval, 2), interval * 4);
    assert_eq!(backoff(interval, 3), interval * 8);
    assert_eq!(backoff(interval, 40), interval * 8, "capped");
}
