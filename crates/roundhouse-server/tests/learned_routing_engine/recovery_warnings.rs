// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The recovery task's held set and its outages (M9 round-2 fixes).
//!
//! **A held session warns once per mark.** The set is keyed by the session
//! and the mark the pending page named, so a new mark is a new episode and
//! warns again, even when the engine's tail, and not the task, delivered the
//! old one. At the end of each full pass the set keeps only the sessions the
//! pass held again, so it stays bounded by the pending set.
//!
//! **A store that is down is an outage, and keeps the cursor.** An apply the
//! learner store refuses as `Unavailable`, and a mark clear the session store
//! fails, each end the sweep on a later page; the next sweep, once the store
//! answers, delivers exactly that page.
//!
//! The warning tests are synchronous with a current-thread runtime, because
//! `captured_warnings` captures only the calling thread's events, and paused
//! where a call hangs, so its timeout is decided on a virtual clock.

use std::num::NonZeroUsize;

use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::contract::LearnerStoreControl;
use roundhouse_core::store::SessionStore;
use roundhouse_server::learner_recovery::{LearnerRecovery, RecoveryCadence};
use roundhouse_server::test_support::captured_warnings;

use crate::recovery::{cadence, idle, pending, recovery, refuse_applies, shadow};
use crate::recovery_holds::{bad_then_good, bounded_sweep, owed};
use crate::rig::{CountingStore, ReplayFault, Rig, RigConfig, admission, residuals};

fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime")
}

/// Run `sweeps` sweeps under `rt` and return what they warned.
fn warned_over(
    rt: &tokio::runtime::Runtime,
    task: &mut LearnerRecovery<CountingStore>,
    sweeps: usize,
) -> String {
    captured_warnings(|| {
        rt.block_on(async {
            for sweep in 0..sweeps {
                idle().await;
                let report = bounded_sweep(task).await;
                assert!(!report.outage, "sweep {sweep}: one session's hold");
            }
        })
    })
}

/// **Item 3: a hold after the engine's tail delivered the session warns
/// again.** The replay hangs, and two sweeps warn once. Then the tail
/// delivers the session and clears its mark, never through the task, and a
/// new turn marks it again. The replay hangs again: that is a new episode
/// under a new mark, and one sweep warns once. Keyed by the session alone,
/// the set still held it from the first episode, and the sweep warned
/// nothing.
#[test]
fn a_hold_after_the_tail_delivered_the_session_warns_again() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-long").await;
        (bad, recovery(&rig))
    });
    let admission = admission("abad", Some(shadow()));
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 0 });
    let warned = warned_over(&rt, &mut task, 2);
    assert_eq!(
        warned.matches("the log replay timed out").count(),
        1,
        "control: two sweeps, one line: {warned}"
    );

    // The tail delivers and clears; the task never sees the session again
    // until t3 marks it.
    rig.sessions.heal_replays(&bad);
    rt.block_on(async {
        rig.turn(&bad, "t2", &admission).await.expect("served");
        assert!(
            !pending(&rig).await.contains(&bad),
            "the tail delivered and cleared the mark"
        );
        refuse_applies(&rig, 1);
        rig.turn(&bad, "t3", &admission).await.expect("served");
        assert!(pending(&rig).await.contains(&bad), "t3 marked it again");
    });
    rig.sessions
        .fault_replays(&bad, ReplayFault::Hangs { after: 0 });
    let warned = warned_over(&rt, &mut task, 1);
    assert_eq!(
        warned.matches("the log replay timed out").count(),
        1,
        "a new mark is a new episode: {warned}"
    );
}

/// **Item 3: the held set does not grow past the pending sessions.** Three
/// sessions are held; the engine's tail then delivers two of them, which the
/// task never confirms. The next full pass keeps only the one still pending.
#[test]
fn the_held_set_is_pruned_to_the_sessions_still_pending() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let held: Vec<SessionId> = ["x", "y", "z"]
        .iter()
        .map(|name| SessionId::new(format!("held/ada/{name}")))
        .collect();
    let mut task = rt.block_on(async {
        let owed_list: Vec<(&SessionId, &str)> = held.iter().map(|s| (s, "held")).collect();
        owed(&rig, &owed_list).await;
        recovery(&rig)
    });
    for session in &held {
        rig.sessions
            .fault_replays(session, ReplayFault::Hangs { after: 0 });
    }
    warned_over(&rt, &mut task, 1);
    assert_eq!(task.held_len(), 3, "control: every session is held");

    rt.block_on(async {
        for session in &held[..2] {
            rig.sessions.heal_replays(session);
            rig.turn(session, "t2", &admission("held", Some(shadow())))
                .await
                .expect("served");
        }
        assert_eq!(pending(&rig).await, held[2..].to_vec());
    });
    warned_over(&rt, &mut task, 1);
    assert_eq!(
        task.held_len(),
        1,
        "a full pass keeps only the sessions still pending"
    );
}

/// **Item 4: a mark clear that hangs on every sweep warns once.** The apply
/// landed, so each later sweep replays nothing and clears again, and the
/// clear hangs again.
#[test]
fn a_mark_clear_that_hangs_on_every_sweep_warns_once() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-clear").await;
        (bad, recovery(&rig))
    });
    rig.sessions.hang_clears(&bad);
    let warned = warned_over(&rt, &mut task, 2);
    assert_eq!(
        warned
            .matches("the learner store applied this session's entries, and acknowledging")
            .count(),
        1,
        "two sweeps, one line: {warned}"
    );
    rt.block_on(async {
        assert!(pending(&rig).await.contains(&bad));
        assert_eq!(residuals(&rig, "good").await, 1);
    });
}

/// **Item 4: an apply that meets a foreign key on every sweep warns once.**
/// It shared the learner store's once-per-outage flag, which the good
/// session's apply behind it reset, so it warned on every sweep.
#[test]
fn an_apply_that_meets_a_foreign_key_on_every_sweep_warns_once() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-foreign").await;
        // A delivered turn: the refused tails behind `bad_then_good` set
        // the store's once-per-outage flag, and a good apply clears it, as
        // the good session's apply does in each sweep.
        rig.turn(
            &SessionId::new("clean/ada/s"),
            "t1",
            &admission("clean", Some(shadow())),
        )
        .await
        .expect("served");
        (bad, recovery(&rig))
    });
    rig.learner.wrong_type_applies_for("abad");
    let warned = warned_over(&rt, &mut task, 2);
    assert_eq!(
        warned
            .matches("the learner store did not take this session's entries")
            .count(),
        1,
        "two sweeps, one line: {warned}"
    );
    rt.block_on(async {
        assert!(pending(&rig).await.contains(&bad));
        assert_eq!(residuals(&rig, "good").await, 1);
    });
}

/// Four sessions owed over two pages of two; the first sweep delivers the
/// first page.
async fn two_pages(rig: &Rig) -> (Vec<SessionId>, LearnerRecovery<CountingStore>) {
    let sessions: Vec<SessionId> = (1..=4)
        .map(|n| SessionId::new(format!("pages/ada/s{n}")))
        .collect();
    let owed_list: Vec<(&SessionId, &str)> = sessions.iter().map(|s| (s, "pages")).collect();
    owed(rig, &owed_list).await;
    let mut task = rig
        .engine
        .learner_recovery(RecoveryCadence {
            max_sessions_per_sweep: NonZeroUsize::new(2).unwrap(),
            ..cadence()
        })
        .expect("a learner");
    idle().await;
    assert_eq!(task.sweep().await.cleared, 2, "the first page");
    assert_eq!(pending(rig).await, sessions[2..].to_vec());
    (sessions, task)
}

/// The store answers again: the next sweep delivers the page the outage
/// stopped, so the cursor did not move past it.
async fn the_stopped_page_is_delivered_next(rig: &Rig, task: &mut LearnerRecovery<CountingStore>) {
    idle().await;
    let report = task.sweep().await;
    assert!(!report.outage);
    assert_eq!(report.cleared, 2, "the page the outage stopped");
    assert!(pending(rig).await.is_empty());
    assert_eq!(residuals(rig, "pages").await, 4, "every session, once each");
}

/// **Item 5: an apply the learner store refuses as `Unavailable` during a
/// sweep is an outage** (`Hold::LearnerUnavailable`): the store is down, and
/// the sweep ends with its cursor on the page it stopped.
#[tokio::test]
async fn an_apply_refused_unavailable_during_a_sweep_is_an_outage_that_keeps_the_cursor() {
    let rig = Rig::new(RigConfig::default());
    let (sessions, mut task) = two_pages(&rig).await;
    refuse_applies(&rig, 1);
    idle().await;
    let report = task.sweep().await;
    assert!(report.outage, "a learner store that refuses is down");
    assert_eq!(pending(&rig).await, sessions[2..].to_vec(), "nothing moved");
    assert!(task.next_delay() > cadence().sweep_interval, "backoff");
    the_stopped_page_is_delivered_next(&rig, &mut task).await;
}

/// **Item 5: a mark clear the session store fails is an outage**
/// (`Hold::AcknowledgementFailed`): the apply landed, and the store that
/// holds the mark is down.
#[tokio::test]
async fn a_mark_clear_that_fails_during_a_sweep_is_an_outage_that_keeps_the_cursor() {
    let rig = Rig::new(RigConfig::default());
    let (sessions, mut task) = two_pages(&rig).await;
    rig.sessions.fail_clears(true);
    idle().await;
    let report = task.sweep().await;
    assert!(report.outage, "a session store that fails a clear is down");
    assert_eq!(pending(&rig).await, sessions[2..].to_vec(), "nothing moved");
    assert!(task.next_delay() > cadence().sweep_interval, "backoff");
    rig.sessions.fail_clears(false);
    the_stopped_page_is_delivered_next(&rig, &mut task).await;
}

/// **Item 2: an unreadable stored mark holds its session, and the sweep
/// delivers the ones behind it.** The page names it rather than failing, and
/// the task warns once for it however many sweeps meet it. Before, the page
/// failed as `Backend`: an outage at that member on every sweep, for every
/// project.
#[test]
fn an_unreadable_mark_is_skipped_warned_once_and_the_sweep_goes_on() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-mark").await;
        rig.sessions.inner.make_learning_mark_unreadable(&bad).await;
        (bad, recovery(&rig))
    });
    let warned = warned_over(&rt, &mut task, 2);
    assert_eq!(
        warned
            .matches("the stored learning mark of this session is unreadable")
            .count(),
        1,
        "two sweeps, one line: {warned}"
    );
    rt.block_on(async {
        assert_eq!(residuals(&rig, "good").await, 1, "the session behind it");
        assert_eq!(residuals(&rig, "abad").await, 0, "nothing of the held one");
        let page = SessionStore::pending_learning(
            rig.sessions.as_ref(),
            None,
            0,
            NonZeroUsize::new(1_000).unwrap(),
        )
        .await
        .expect("the index reads");
        assert_eq!(page.unreadable, vec![bad], "it stays, named as unreadable");
    });
    assert_eq!(task.next_delay(), cadence().sweep_interval, "no backoff");
}

/// **M9 round-3, item 1: an orphaned pending member holds its session, and
/// the sweep delivers the ones behind it.** A foreign `HDEL` of the marks
/// field, not just an unparseable value, still lands in the same
/// `unreadable` bucket, so the page names it rather than failing, and the
/// task warns once for it however many sweeps meet it -- the same bound as
/// an unparseable mark.
#[test]
fn an_orphaned_pending_member_is_skipped_warned_once_and_the_sweep_goes_on() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-orphan").await;
        rig.sessions.inner.orphan_pending_learning_mark(&bad).await;
        (bad, recovery(&rig))
    });
    let warned = warned_over(&rt, &mut task, 2);
    assert_eq!(
        warned
            .matches("the stored learning mark of this session is unreadable")
            .count(),
        1,
        "two sweeps, one line: {warned}"
    );
    rt.block_on(async {
        assert_eq!(residuals(&rig, "good").await, 1, "the session behind it");
        assert_eq!(residuals(&rig, "abad").await, 0, "nothing of the held one");
        let page = SessionStore::pending_learning(
            rig.sessions.as_ref(),
            None,
            0,
            NonZeroUsize::new(1_000).unwrap(),
        )
        .await
        .expect("the index reads");
        assert_eq!(page.unreadable, vec![bad], "it stays, named as unreadable");
    });
    assert_eq!(task.next_delay(), cadence().sweep_interval, "no backoff");
}

/// **Item 2: a clear that finds the mark unreadable holds that session.** The
/// mark was readable when the page named it and is not when the clear reads
/// it: `CorruptLog`, one session's data, and not the session store being
/// down. Before, every clear error was `AcknowledgementFailed`, an outage.
#[test]
fn a_clear_that_finds_the_mark_unreadable_holds_its_session() {
    let rt = paused_runtime();
    let rig = Rig::new(RigConfig::default());
    let (bad, mut task) = rt.block_on(async {
        let bad = bad_then_good(&rig, "a-mark").await;
        (bad, recovery(&rig))
    });
    rig.sessions.corrupt_mark(&bad);
    let warned = warned_over(&rt, &mut task, 2);
    assert_eq!(
        warned
            .matches("the learner store applied this session's entries, and acknowledging")
            .count(),
        1,
        "two sweeps, one line: {warned}"
    );
    rt.block_on(async {
        assert_eq!(residuals(&rig, "good").await, 1, "the session behind it");
        assert!(pending(&rig).await.contains(&bad));
    });
    assert_eq!(task.next_delay(), cadence().sweep_interval, "no backoff");
}

/// **Item 2: an audit requeue that finds the mark unreadable skips that
/// session** and audits the ones behind it. Both sessions were delivered and
/// the learner store lost both projects; the first one's requeue answers
/// `CorruptLog`.
#[tokio::test]
async fn an_audit_requeue_that_finds_the_mark_unreadable_skips_that_session() {
    let rig = Rig::new(RigConfig::default());
    let bad = SessionId::new("a-audit/ada/s");
    let good = SessionId::new("good/ada/s");
    let mut lost = Vec::new();
    for (session, name) in [(&bad, "abad"), (&good, "good")] {
        let before = rig.learner.inner.snapshot(&crate::rig::project(name)).await;
        rig.turn(session, "t1", &admission(name, Some(shadow())))
            .await
            .expect("served");
        lost.push((name, before));
    }
    assert!(pending(&rig).await.is_empty(), "the tail delivered both");
    for (name, before) in lost {
        rig.learner
            .inner
            .restore(&crate::rig::project(name), before)
            .await;
    }
    rig.sessions.corrupt_mark(&bad);
    let mut task = recovery(&rig);
    let report = task.sweep().await;
    assert!(!report.outage, "one session's mark is not an outage");
    assert_eq!(report.requeued, 1, "the session behind it is requeued");
    assert_eq!(pending(&rig).await, vec![good]);
}
