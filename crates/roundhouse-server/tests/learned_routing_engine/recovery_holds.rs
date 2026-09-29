// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The recovery task's hold classes (M9 review): which trouble ends a sweep
//! as an outage, and which holds one session while the sweep goes on.
//!
//! **Only a store that is down is an outage.** It ends the sweep with the
//! pass cursor where it was and backs off. Everything that belongs to one
//! session (a refusal no retry fixes, a gap the backfill cannot close, a log
//! that is gone, a replay, apply, backfill or clear that runs past its
//! timeout) holds that session, and the sweep delivers the sessions behind it
//! with no backoff. Each hold test puts its session first in byte order
//! (`a-...`) and a good one behind it (`good/...`).
//!
//! A test whose session hangs runs under paused time, so the task's timeouts
//! are decided on a virtual clock, and wraps its sweep in [`HANG`]: a sweep
//! that waits on a hung call fails that bound instead of hanging the suite.

use std::num::NonZeroUsize;
use std::time::Duration;

use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::LearnerError;
use roundhouse_server::learner_recovery::{LearnerRecovery, RecoveryCadence, SweepReport};
use roundhouse_server::test_support::captured_warnings;

use crate::recovery::{cadence, idle, pending, recovery, refuse_applies, shadow};
use crate::rig::{CountingStore, ReplayFault, Rig, RigConfig, admission, residuals};

/// Longer than every timeout in [`cadence`]: a stall the task must bound.
const HANG: Duration = Duration::from_secs(3_600);

/// Each session owes one entry, in the project named beside it.
async fn owed(rig: &Rig, sessions: &[(&SessionId, &str)]) {
    refuse_applies(rig, sessions.len());
    for (session, project) in sessions {
        rig.turn(session, "t1", &admission(project, Some(shadow())))
            .await
            .expect("served");
    }
}

/// One sweep, bounded by [`HANG`].
async fn bounded_sweep(task: &mut LearnerRecovery<CountingStore>) -> SweepReport {
    tokio::time::timeout(HANG, task.sweep())
        .await
        .expect("the sweep is bounded: no call it waits on may hang it")
}

/// `bad` is held, and the sweep delivered `good`'s project behind it with no
/// outage and no backoff.
async fn assert_held_and_the_sweep_went_on(
    rig: &Rig,
    task: &LearnerRecovery<CountingStore>,
    report: SweepReport,
    bad: &SessionId,
) {
    assert!(!report.outage, "one session's trouble is not an outage");
    assert_eq!(
        residuals(rig, "good").await,
        1,
        "the session behind it is delivered"
    );
    assert_eq!(task.next_delay(), cadence().sweep_interval, "no backoff");
    assert!(
        pending(rig).await.contains(bad),
        "the held session stays pending"
    );
}

/// The bad session first in byte order, a good one behind it, both owed.
async fn bad_then_good(rig: &Rig, bad: &str) -> SessionId {
    let bad = SessionId::new(format!("{bad}/ada/s"));
    let good = SessionId::new("good/ada/s");
    owed(rig, &[(&bad, "abad"), (&good, "good")]).await;
    bad
}

/// **L1: a session store that goes down after the index page answered is an
/// outage.** The replay of the page's first session fails with a backend
/// error: the sweep ends there with its cursor where it was, one warning
/// covers it, and no session is warned about or skipped for a pass. The next
/// sweep after the store returns delivers exactly that page.
#[test]
fn a_session_store_failure_after_the_index_page_is_an_outage_that_keeps_the_cursor() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let rig = Rig::new(RigConfig::default());
    let sessions: Vec<SessionId> = ["a", "b", "c", "d"]
        .iter()
        .map(|name| SessionId::new(format!("down/ada/{name}")))
        .collect();
    let mut task = rt.block_on(async {
        let owed_list: Vec<(&SessionId, &str)> = sessions.iter().map(|s| (s, "down")).collect();
        owed(&rig, &owed_list).await;
        rig.engine
            .learner_recovery(RecoveryCadence {
                max_sessions_per_sweep: NonZeroUsize::new(2).unwrap(),
                ..cadence()
            })
            .expect("a learner")
    });
    rig.sessions.go_down_after_the_next_index_page(true);
    let warned = captured_warnings(|| {
        rt.block_on(async {
            idle().await;
            let report = task.sweep().await;
            assert!(report.outage, "a session store that is down is an outage");
            assert_eq!(report.cleared, 0);
        });
    });
    assert_eq!(
        warned
            .matches("the learner recovery task cannot reach a store")
            .count(),
        1,
        "one line for the outage: {warned}"
    );
    assert!(
        !warned.contains("the log replay failed"),
        "and none per session: {warned}"
    );
    assert!(task.next_delay() > cadence().sweep_interval, "backoff");

    rig.sessions.go_down_after_the_next_index_page(false);
    rt.block_on(async {
        idle().await;
        let report = task.sweep().await;
        assert!(!report.outage);
        assert_eq!(report.cleared, 2);
        assert_eq!(
            pending(&rig).await,
            sessions[2..].to_vec(),
            "the page the outage stopped is the page the next sweep delivers"
        );
    });
}

/// **L2: an apply that times out holds its session.** Its result is unknown,
/// and the next session's watermark read is what probes whether the store is
/// down, so one slow session must not back the task off.
#[tokio::test(start_paused = true)]
async fn an_apply_that_times_out_holds_its_session_and_the_sweep_goes_on() {
    let rig = Rig::new(RigConfig::default());
    let bad = bad_then_good(&rig, "a-slow").await;
    rig.learner.stall_session(&bad, HANG);
    idle().await;
    let mut task = recovery(&rig);
    let report = bounded_sweep(&mut task).await;
    assert_held_and_the_sweep_went_on(&rig, &task, report, &bad).await;
}

/// **L4: a mark clear is bounded by the source timeout.** The apply landed,
/// the clear hangs: the session is held, and the sweep goes on.
#[tokio::test(start_paused = true)]
async fn a_mark_clear_that_hangs_is_bounded_and_holds_its_session() {
    let rig = Rig::new(RigConfig::default());
    let bad = bad_then_good(&rig, "a-clear").await;
    rig.sessions.hang_clears(&bad);
    idle().await;
    let mut task = recovery(&rig);
    let report = bounded_sweep(&mut task).await;
    assert_eq!(residuals(&rig, "abad").await, 1, "the apply landed");
    assert_held_and_the_sweep_went_on(&rig, &task, report, &bad).await;
}

/// **L4 and L5: a gap backfill is bounded by the source timeout**, and a
/// backfill that cannot replay (`SourceUnavailable`) holds its session. The
/// first replay answers, the store reports a gap, and the backfill's replay
/// hangs.
#[tokio::test(start_paused = true)]
async fn a_backfill_that_hangs_is_bounded_and_holds_its_session() {
    let rig = Rig::new(RigConfig::default());
    let bad = bad_then_good(&rig, "a-gap").await;
    rig.learner
        .refuse_session(&bad, LearnerError::ChainGap { store_watermark: 0 });
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 1 });
    idle().await;
    let mut task = recovery(&rig);
    let report = bounded_sweep(&mut task).await;
    assert!(rig.sessions.replays(&bad) >= 2, "the backfill ran");
    assert_held_and_the_sweep_went_on(&rig, &task, report, &bad).await;
}

/// **L1: a gap backfill that meets a session-store failure is an outage**
/// (`Hold::SourceDown`), like the replay before it: the sweep ends at that
/// session, and the session behind it waits for the next sweep, which starts
/// at the same place.
#[tokio::test]
async fn a_backfill_that_meets_a_store_failure_is_an_outage_that_keeps_the_cursor() {
    let rig = Rig::new(RigConfig::default());
    let bad = SessionId::new("a-gap/ada/s");
    let behind = SessionId::new("b-good/ada/s");
    let last = SessionId::new("c-good/ada/s");
    owed(&rig, &[(&bad, "abad"), (&behind, "good"), (&last, "last")]).await;
    rig.learner
        .refuse_session(&bad, LearnerError::ChainGap { store_watermark: 0 });
    rig.sessions
        .fault_replays(&bad, ReplayFault::Fails { after: 1 });
    let mut task = rig
        .engine
        .learner_recovery(RecoveryCadence {
            max_sessions_per_sweep: NonZeroUsize::new(2).unwrap(),
            ..cadence()
        })
        .expect("a learner");

    idle().await;
    let report = task.sweep().await;
    assert!(report.outage, "the backfill met a store that is down");
    assert_eq!(residuals(&rig, "good").await, 0, "the sweep ended there");
    assert!(task.next_delay() > cadence().sweep_interval, "backoff");

    // The store returns; the gap stays, so the session is now held.
    rig.sessions.heal_replays(&bad);
    idle().await;
    let report = task.sweep().await;
    assert!(!report.outage);
    assert_eq!(residuals(&rig, "good").await, 1, "the page it stopped on");
    assert_eq!(
        pending(&rig).await,
        vec![bad.clone(), last.clone()],
        "the cursor did not move past the page the outage stopped"
    );
}

/// **L5: a refusal no retry fixes (`Malformed`) stops its session**, and the
/// stop is not an outage.
#[tokio::test]
async fn a_malformed_refusal_holds_its_session_and_the_sweep_goes_on() {
    let rig = Rig::new(RigConfig::default());
    let bad = bad_then_good(&rig, "a-malformed").await;
    rig.learner.refuse_session(
        &bad,
        LearnerError::Malformed {
            reason: "the fixture refuses this session".into(),
        },
    );
    idle().await;
    let mut task = recovery(&rig);
    let report = task.sweep().await;
    assert_held_and_the_sweep_went_on(&rig, &task, report, &bad).await;
}

/// **L5: a gap the backfill does not close (`GapPersisted`)** holds its
/// session, and is not an outage.
#[tokio::test]
async fn a_gap_the_backfill_cannot_close_holds_its_session_and_the_sweep_goes_on() {
    let rig = Rig::new(RigConfig::default());
    let bad = bad_then_good(&rig, "a-gap").await;
    rig.learner
        .refuse_session(&bad, LearnerError::ChainGap { store_watermark: 0 });
    idle().await;
    let mut task = recovery(&rig);
    let report = task.sweep().await;
    assert_held_and_the_sweep_went_on(&rig, &task, report, &bad).await;
}

/// **L5: a marked session whose log is gone** (`SessionNotFound` on the
/// replay) holds that session, and is not an outage.
#[tokio::test]
async fn a_log_gone_from_the_store_holds_its_session_and_the_sweep_goes_on() {
    let rig = Rig::new(RigConfig::default());
    let bad = bad_then_good(&rig, "a-gone").await;
    rig.sessions
        .fault_replays(&bad, ReplayFault::Missing { after: 0 });
    idle().await;
    let mut task = recovery(&rig);
    let report = task.sweep().await;
    assert_held_and_the_sweep_went_on(&rig, &task, report, &bad).await;
}

/// **A replay that times out on every sweep warns once**, not once per
/// sweep, and the held set forgets the session once it is delivered. Also
/// L5's replay-timeout hold: not an outage.
///
/// Synchronous with a current-thread runtime, because `captured_warnings`
/// captures only the calling thread's events; paused, so the replay timeout
/// is decided on a virtual clock.
#[test]
fn a_replay_that_times_out_on_every_sweep_warns_once() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-long").await;
        (bad, recovery(&rig))
    });
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 0 });
    let warned = captured_warnings(|| {
        rt.block_on(async {
            for sweep in 0..2 {
                idle().await;
                let report = bounded_sweep(&mut task).await;
                assert!(!report.outage, "sweep {sweep}: a slow log is not an outage");
            }
            assert_held_and_the_sweep_went_on(&rig, &task, SweepReport::default(), &bad).await;
        });
    });
    assert_eq!(
        warned.matches("the log replay timed out").count(),
        1,
        "two sweeps, one line: {warned}"
    );

    // Delivered once the log replays in time, and forgotten: a later hang
    // is a new episode and warns again.
    rig.sessions.heal_replays(&bad);
    rt.block_on(async {
        idle().await;
        bounded_sweep(&mut task).await;
        assert_eq!(residuals(&rig, "abad").await, 1, "delivered once healed");
    });
    rt.block_on(async {
        refuse_applies(&rig, 1);
        rig.turn(&bad, "t2", &admission("abad", Some(shadow())))
            .await
            .expect("served");
    });
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 0 });
    let warned = captured_warnings(|| {
        rt.block_on(async {
            idle().await;
            bounded_sweep(&mut task).await;
        });
    });
    assert_eq!(
        warned.matches("the log replay timed out").count(),
        1,
        "a new episode after a delivery warns again: {warned}"
    );
}

/// **Mutation survivor S1: the cursor stays on the page an outage stopped.**
/// With more sessions pending than one sweep examines, the outage hits the
/// second page. When the store returns, the next sweep delivers exactly that
/// page: a task that moved its cursor on the outage would deliver the third
/// page first and skip the second for a whole pass.
#[tokio::test]
async fn an_outage_in_a_later_page_keeps_the_cursor_on_the_page_it_stopped() {
    let rig = Rig::new(RigConfig::default());
    let sessions: Vec<SessionId> = (1..=6)
        .map(|n| SessionId::new(format!("paged/ada/s{n}")))
        .collect();
    let owed_list: Vec<(&SessionId, &str)> = sessions.iter().map(|s| (s, "paged")).collect();
    owed(&rig, &owed_list).await;
    let mut task = rig
        .engine
        .learner_recovery(RecoveryCadence {
            max_sessions_per_sweep: NonZeroUsize::new(2).unwrap(),
            ..cadence()
        })
        .expect("a learner");

    idle().await;
    assert_eq!(task.sweep().await.cleared, 2, "the first page");
    assert_eq!(pending(&rig).await, sessions[2..].to_vec());

    rig.learner.fail_watermarks(true);
    idle().await;
    assert!(
        task.sweep().await.outage,
        "the second page meets the outage"
    );
    rig.learner.fail_watermarks(false);

    idle().await;
    let report = task.sweep().await;
    assert_eq!(report.cleared, 2);
    assert_eq!(
        pending(&rig).await,
        sessions[4..].to_vec(),
        "the second page, not the third"
    );
    idle().await;
    task.sweep().await;
    assert!(pending(&rig).await.is_empty());
    assert_eq!(
        residuals(&rig, "paged").await,
        6,
        "every session, once each"
    );
}

/// **Mutation survivor S3: a session the engine's tail stopped is not
/// delivered again by the recovery task.** The tail's apply was refused
/// `Malformed`, so the engine stopped the session and its mark stays. The
/// task shares the engine's stopped set: the sweep spends no replay and no
/// apply on it, and delivers the session behind it.
#[tokio::test]
async fn a_session_the_engine_stopped_is_not_delivered_again_by_the_recovery_task() {
    let rig = Rig::new(RigConfig::default());
    let stopped = SessionId::new("a-stopped/ada/s");
    rig.learner.refuse_session(
        &stopped,
        LearnerError::Malformed {
            reason: "the fixture refuses this session".into(),
        },
    );
    rig.turn(&stopped, "t1", &admission("abad", Some(shadow())))
        .await
        .expect("served");
    assert_eq!(pending(&rig).await, vec![stopped.clone()], "the mark stays");
    let good = SessionId::new("good/ada/s");
    owed(&rig, &[(&good, "good")]).await;

    let (replays, applies) = (rig.sessions.replays(&stopped), rig.learner.applies());
    idle().await;
    let mut task = recovery(&rig);
    let report = task.sweep().await;
    assert_eq!(
        rig.sessions.replays(&stopped),
        replays,
        "a stopped session costs the log no replay"
    );
    assert_eq!(
        rig.learner.applies(),
        applies + 1,
        "and the store no apply: the one apply is the good session's"
    );
    assert_held_and_the_sweep_went_on(&rig, &task, report, &stopped).await;
}

/// **A gap backfill that fails on every sweep warns once**, through the same
/// held set as the replay, and warns again after the session is delivered.
/// The engine's tail passes no set and warns every time, as before.
#[test]
fn a_backfill_that_fails_on_every_sweep_warns_once() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-gap").await;
        (bad, recovery(&rig))
    });
    rig.learner
        .refuse_session(&bad, LearnerError::ChainGap { store_watermark: 0 });
    // Every replay but the first of each sweep: the backfill's.
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 1 });
    let warned = captured_warnings(|| {
        rt.block_on(async {
            idle().await;
            let report = bounded_sweep(&mut task).await;
            assert!(!report.outage);
            assert_eq!(residuals(&rig, "good").await, 1);
        });
    });
    assert_eq!(
        warned
            .matches("the learning backfill could not replay the log")
            .count(),
        1,
        "control: the first sweep warns: {warned}"
    );
    // The second sweep's first replay is the fault's second read too, so it
    // hangs before the backfill: re-arm so the replay answers again.
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 1 });
    let warned = captured_warnings(|| {
        rt.block_on(async {
            idle().await;
            let report = bounded_sweep(&mut task).await;
            assert!(!report.outage);
        });
    });
    assert_eq!(
        warned
            .matches("the learning backfill could not replay the log")
            .count(),
        0,
        "the second sweep does not warn again: {warned}"
    );
}
