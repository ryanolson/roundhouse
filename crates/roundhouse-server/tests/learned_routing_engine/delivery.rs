// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Delivery through the `run_turn` tail: each entry the log owes the store
//! lands once, through a refused or lost acknowledgement, a timeout, a
//! successor, a steered turn, a gap and a diverged chain.
//!
//! Every completed learned turn adds one latency residual to the target that
//! served it, so `large`'s residual count is the number of turn entries the
//! store holds. A duplicate would count twice.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use roundhouse_core::event::SessionEventKind;
use roundhouse_core::event::{ControlRecord, Usage, ValidationOutcome};
use roundhouse_core::ids::SessionId;
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::learn_store::contract::LearnerStoreControl;
use roundhouse_core::learn_store::{LearnerError, LearnerStore, LearningBatch};
use roundhouse_core::routing::learn::{LearnerMode, LearnerTerms, Strategy};
use roundhouse_core::session::LEARNING_PAGE;
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{
    ArmShares, IntervalLabel, ValidationTerms, Validator, ValidatorConfig,
};
use roundhouse_server::Admission;

use crate::common::validate::{AlwaysFires, ON_TRACK, ScriptedJudge, open_trigger};
use crate::rig::{
    ApplyScript, Rig, RigConfig, SALT, admission, delivery, fresh_input, large, project,
    replayed_chain, residuals, terms, units_over_keys,
};

fn shadow() -> LearnerTerms {
    terms(LearnerMode::Shadow)
}

fn admission_with_default_timeouts(project: &str) -> Admission {
    admission(project, Some(shadow()))
}

/// One read in `plan`, and in the tail one apply, one mark clear and one
/// `LearningApplied` append, per learner turn with entries pending. The keys
/// the read visits are the keys the decision records.
#[tokio::test]
async fn a_learner_turn_makes_one_read_one_apply_one_clear_and_one_append() {
    let rig = Rig::new(RigConfig::default());
    rig.learner.inner.record_visits();
    let session = SessionId::new("count/ada/s");
    let admission = admission("count", Some(shadow()));
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(
        (
            rig.learner.reads(),
            rig.learner.applies(),
            rig.sessions.clears(),
            rig.sessions.acks()
        ),
        (1, 1, 1, 1)
    );
    let record = rig.last_learned(&session).await;
    let visited = rig.learner.inner.take_visited();
    for key in record.input.keys() {
        assert!(
            visited
                .iter()
                .any(|name| name.ends_with(&format!(":{}", key.part()))
                    && name.contains(&format!(":q:{}:", key.level().label()))),
            "the read visits the key the decision records, {key}: {visited:?}"
        );
    }
    rig.turn(&session, "t2", &admission).await.expect("served");
    assert_eq!(
        (
            rig.learner.reads(),
            rig.learner.applies(),
            rig.sessions.clears(),
            rig.sessions.acks()
        ),
        (2, 2, 2, 2)
    );
    let acknowledged = rig.acknowledged(&session).await;
    assert_eq!(acknowledged.len(), 2);
    assert_eq!(residuals(&rig, "count").await, 2);
}

fn reviewing(judge: Arc<ScriptedJudge>) -> Arc<dyn Interjector> {
    Arc::new(
        Validator::new(
            judge,
            ValidatorConfig {
                trigger: roundhouse_core::validate::TriggerConfig {
                    max_validations_per_session: 1_000,
                    max_consecutive_interventions: 1_000,
                    ..open_trigger()
                },
                arm_salt: SALT.into(),
                ..ValidatorConfig::default()
            },
        )
        .with_signals(vec![Box::new(AlwaysFires)]),
    )
}

fn judged(admission: Admission) -> Admission {
    Admission {
        validation: Some(ValidationTerms {
            shares: ArmShares::new(0, 1, 0).expect("one weight is a table"),
            action: Default::default(),
            placebo_rate: 1.0,
            handoff_note: None,
        }),
        ..admission
    }
}

/// A positive review credits the strategies that agreed with the served
/// target once: across the turn that delivers it, a successor's reopen, and a
/// replayed resend of the whole chain.
#[tokio::test]
async fn a_positive_review_updates_the_project_once_across_reopen_and_replay() {
    let judge = ScriptedJudge::answering(&[ON_TRACK]);
    let rig = Rig::new(RigConfig {
        interjector: Some(reviewing(judge)),
        ..RigConfig::default()
    });
    let session = SessionId::new("review/ada/s");
    let admission = judged(admission("review", Some(shadow())));
    rig.turn(&session, "t1", &admission).await.expect("served");
    rig.turn(&session, "t2", &admission).await.expect("served");
    let labels: Vec<IntervalLabel> = rig
        .events(&session)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            roundhouse_core::event::SessionEventKind::ValidationDecided {
                outcome:
                    ValidationOutcome::Judged {
                        interval: Some(review),
                        ..
                    },
                ..
            } => Some(review.label),
            _ => None,
        })
        .collect();
    assert_eq!(labels, vec![IntervalLabel::Positive], "one parsed review");

    // `CREDIT_SCALE` units at each of the three levels, all on this session's
    // one key per level.
    let input = fresh_input();
    let credited = |strategy| units_over_keys(&rig.learner, "review", &input, strategy);
    assert_eq!(credited(Strategy::Rules).await, (3_000, 3_000));
    assert_eq!(credited(Strategy::Capable).await, (3_000, 3_000));
    assert_eq!(
        credited(Strategy::Efficient).await,
        (0, 0),
        "efficient's plan served small, not the target the interval served"
    );

    // A successor reopens the session and serves a turn; nothing reviews it.
    let successor = Rig::over(
        RigConfig::default(),
        Arc::clone(&rig.sessions),
        Arc::clone(&rig.learner),
    );
    successor
        .turn(&session, "t3", &admission)
        .await
        .expect("served");
    assert_eq!(credited(Strategy::Rules).await, (3_000, 3_000));

    // And a resend of the whole replayed chain applies nothing.
    let chain = replayed_chain(&rig, &session).await;
    let project = project("review");
    let applied = rig
        .learner
        .inner
        .apply(&LearningBatch {
            project: &project,
            session: &session,
            entries: &chain,
        })
        .await
        .expect("a resend is accepted");
    assert_eq!(
        applied.applied, 0,
        "every entry is at or below the watermark"
    );
    assert_eq!(credited(Strategy::Rules).await, (3_000, 3_000));
}

/// The store applied an entry and the acknowledgement append was refused.
/// The next turn resends it with a new one, and only the new one applies.
#[tokio::test]
async fn a_refused_ack_then_a_new_entry_applies_only_the_new_entry() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("refused/ada/s");
    let admission = admission("refused", Some(shadow()));
    rig.sessions.refuse_acks(true);
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert!(rig.acknowledged(&session).await.is_empty());
    assert_eq!(residuals(&rig, "refused").await, 1, "the apply landed");

    rig.sessions.refuse_acks(false);
    rig.turn(&session, "t2", &admission).await.expect("served");
    assert_eq!(
        residuals(&rig, "refused").await,
        2,
        "the resent entry did not apply twice"
    );
    let delivery = delivery(&rig);
    assert_eq!(
        (delivery.applied_entries, delivery.duplicate_entries),
        (2, 1)
    );
    assert_eq!(delivery.acknowledgement_failures, 1);
    assert_eq!(rig.acknowledged(&session).await.len(), 1);
}

/// An apply that landed and did not answer within `apply_timeout_ms`: no
/// acknowledgement, and the retry applies each entry once.
#[tokio::test]
async fn an_apply_timeout_then_retry_applies_each_entry_once() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("stall/ada/s");
    let admission = Admission {
        learner_apply_timeout_ms: Some(50),
        ..admission("stall", Some(shadow()))
    };
    rig.learner
        .script(ApplyScript::LandThenStall(Duration::from_millis(600)));
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert!(
        rig.acknowledged(&session).await.is_empty(),
        "no acknowledgement"
    );
    assert_eq!(residuals(&rig, "stall").await, 1);
    assert_eq!(delivery(&rig).timed_out, 1);

    // The retry runs under the ordinary timeout, so a loaded machine cannot
    // drop the real apply this test counts.
    rig.turn(&session, "t2", &admission_with_default_timeouts("stall"))
        .await
        .expect("served");
    assert_eq!(residuals(&rig, "stall").await, 2);
    assert_eq!(rig.acknowledged(&session).await.len(), 1);
}

/// A node applies and loses its acknowledgement; a successor opens the
/// session, finds the entry still above its hint, and applies only its own.
#[tokio::test]
async fn a_successor_after_apply_without_ack_applies_only_its_new_entries() {
    let first = Rig::new(RigConfig::default());
    let session = SessionId::new("heir/ada/s");
    let admission = admission("heir", Some(shadow()));
    first.sessions.refuse_acks(true);
    first
        .turn(&session, "t1", &admission)
        .await
        .expect("served");
    first.sessions.refuse_acks(false);

    let successor = Rig::over(
        RigConfig::default(),
        Arc::clone(&first.sessions),
        Arc::clone(&first.learner),
    );
    successor
        .turn(&session, "t2", &admission)
        .await
        .expect("served");
    assert_eq!(residuals(&successor, "heir").await, 2);
    let delivery = delivery(&successor);
    assert_eq!(
        (delivery.applied_entries, delivery.duplicate_entries),
        (1, 1)
    );
}

/// Answers the second turn at the seam and proceeds otherwise.
struct SteerSecond {
    calls: AtomicUsize,
}

#[async_trait]
impl Interjector for SteerSecond {
    async fn consider(&self, _context: &InterjectionContext<'_>) -> Interjection {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            1 => Interjection::Complete {
                item: Item {
                    role: Role::Assistant,
                    content: ItemContent::Text {
                        text: "steered".into(),
                    },
                    response_id: None,
                },
                usage: Usage::default(),
                record: ControlRecord::default(),
            },
            _ => Interjection::proceed(),
        }
    }
}

/// A turn the seam answered is still a turn tail: the entries a failed
/// delivery left pending are delivered on it.
#[tokio::test]
async fn a_steered_turn_still_applies_pending_entries() {
    let rig = Rig::new(RigConfig {
        interjector: Some(Arc::new(SteerSecond {
            calls: AtomicUsize::new(0),
        })),
        ..RigConfig::default()
    });
    let session = SessionId::new("steer/ada/s");
    let admission = admission("steer", Some(shadow()));
    rig.learner
        .script(ApplyScript::Refuse(LearnerError::Unavailable(
            "down".into(),
        )));
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(
        residuals(&rig, "steer").await,
        0,
        "the first delivery failed"
    );

    let steered = rig.turn(&session, "t2", &admission).await.expect("steered");
    assert!(
        steered.decision.is_none(),
        "the seam answered, nothing routed"
    );
    assert_eq!(rig.learner.applies(), 2);
    assert_eq!(
        residuals(&rig, "steer").await,
        1,
        "t1's entry landed on the steered tail"
    );
    assert_eq!(rig.acknowledged(&session).await.len(), 1);
}

/// Delivery follows the log, not the project's current mode (draft section
/// 23): a session with learned history whose project is turned `off` still
/// has its pending entries delivered, and makes no store read for its turns.
#[tokio::test]
async fn a_project_turned_off_still_delivers_its_sessions_pending_entries() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("dim/ada/s");
    rig.learner
        .script(ApplyScript::Refuse(LearnerError::Unavailable(
            "down".into(),
        )));
    rig.turn(&session, "t1", &admission("dim", Some(shadow())))
        .await
        .expect("served");
    assert_eq!(residuals(&rig, "dim").await, 0, "the first delivery failed");
    assert_eq!(rig.learner.reads(), 1);

    rig.turn(
        &session,
        "t2",
        &admission("dim", Some(terms(LearnerMode::Off))),
    )
    .await
    .expect("served");
    assert_eq!(rig.learner.reads(), 1, "an off turn reads nothing");
    assert_eq!(rig.learner.applies(), 2, "and its tail still delivers");
    assert_eq!(
        residuals(&rig, "dim").await,
        1,
        "t1's entry; t2 was not learned"
    );
    assert_eq!(rig.acknowledged(&session).await.len(), 1);
}

/// A `ChainDiverged` refusal stops the session's delivery: its entries stay
/// pending under their mark, the stop is counted, and no later tail backfills
/// or calls the store for that session.
#[tokio::test]
async fn a_chain_diverged_apply_leaves_the_entries_pending_records_the_stop_and_never_backfills() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("fork/ada/s");
    let admission = admission("fork", Some(shadow()));
    rig.learner
        .script(ApplyScript::Refuse(LearnerError::ChainDiverged {
            store_watermark: 0,
        }));
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(rig.learner.applies(), 1);
    // Two more turns: a stop that is spent by the first check after it would
    // let the third call the store again.
    rig.turn(&session, "t2", &admission).await.expect("served");
    rig.turn(&session, "t3", &admission).await.expect("served");
    assert_eq!(
        rig.learner.applies(),
        1,
        "the stopped session calls the store no more"
    );

    let delivery = delivery(&rig);
    assert_eq!(delivery.diverged, 1);
    assert_eq!(
        delivery.backfills, 0,
        "a diverged chain is never backfilled"
    );
    assert!(rig.acknowledged(&session).await.is_empty());
    assert!(
        !replayed_chain(&rig, &session).await.is_empty(),
        "the entries are still owed"
    );
    let pending = rig
        .sessions
        .pending_learning(None, 0, std::num::NonZeroUsize::new(16).unwrap())
        .await
        .expect("the index reads");
    assert!(
        pending
            .sessions
            .iter()
            .any(|marked| marked.session_id == session),
        "the source mark stays for the recovery task: {pending:?}"
    );
    // Control: another session of the project delivers.
    let other = SessionId::new("fork/ada/other");
    rig.turn(&other, "t1", &admission).await.expect("served");
    assert_eq!(rig.learner.applies(), 2);
    assert_eq!(rig.acknowledged(&other).await.len(), 1);
}

/// The store lost what it acknowledged: the next page starts above its
/// watermark, the store answers with a gap, and one backfill refills the
/// page from the store's watermark in the same tail.
#[tokio::test]
async fn a_gap_is_backfilled_once_and_applied_in_the_same_tail() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("gap/ada/s");
    let admission = admission("gap", Some(shadow()));
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(residuals(&rig, "gap").await, 1);
    // The learner store loses the project.
    rig.learner.inner.restore(&project("gap"), None).await;
    assert_eq!(residuals(&rig, "gap").await, 0);

    rig.turn(&session, "t2", &admission).await.expect("served");
    assert_eq!(residuals(&rig, "gap").await, 2, "both entries, once each");
    let delivery = delivery(&rig);
    assert_eq!((delivery.gaps, delivery.backfills), (1, 1));
    assert_eq!(
        rig.learner.applies(),
        3,
        "t1, then t2's gap, then the backfilled page"
    );
    assert_eq!(rig.acknowledged(&session).await.len(), 2);
}

/// A page that ran dry is refilled from the hint, and the refilled page can
/// still meet a gap when the store lost what it acknowledged. The refill does
/// not spend the gap's backfill: each tail refills once and backfills the
/// gap once, so the gap closes in the same tail.
#[tokio::test]
async fn a_gap_after_a_dry_page_refill_is_backfilled_in_the_same_tail() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("dry/ada/s");
    let admission = admission("dry", Some(shadow()));
    // `LEARNING_PAGE + 1` turns whose deliveries all fail: the page holds
    // `LEARNING_PAGE` entries, and one more is owed beyond it.
    let backlog = LEARNING_PAGE + 1;
    for _ in 0..backlog {
        rig.learner
            .script(ApplyScript::Refuse(LearnerError::Unavailable(
                "down".into(),
            )));
    }
    for turn in 0..backlog {
        rig.turn(&session, &format!("b{turn}"), &admission)
            .await
            .expect("served");
    }
    assert_eq!(residuals(&rig, "dry").await, 0);
    // One tail delivers the full page and acknowledges it. The live page is
    // now dry, with this turn's entry and the one beyond it still owed.
    rig.turn(&session, "fill", &admission)
        .await
        .expect("served");
    assert_eq!(residuals(&rig, "dry").await, LEARNING_PAGE as u64);
    // The learner store loses the project.
    rig.learner.inner.restore(&project("dry"), None).await;

    rig.turn(&session, "a", &admission).await.expect("served");
    rig.turn(&session, "b", &admission).await.expect("served");
    let entries = (backlog + 3) as u64;
    assert_eq!(
        residuals(&rig, "dry").await,
        entries,
        "every entry, once each"
    );
    let delivery = delivery(&rig);
    assert_eq!(
        (delivery.gaps, delivery.backfills),
        (1, 3),
        "a: a refill, the gap and its backfill; b: a refill"
    );
}

/// At most one gap backfill per tail: a store that answers the backfilled
/// page with another gap is not asked again in that tail, and the entries
/// wait for the next one.
#[tokio::test]
async fn a_second_gap_in_one_tail_is_not_backfilled_again() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("gaps/ada/s");
    let admission = admission("gaps", Some(shadow()));
    for _ in 0..2 {
        rig.learner
            .script(ApplyScript::Refuse(LearnerError::ChainGap {
                store_watermark: 0,
            }));
    }
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(
        rig.learner.applies(),
        2,
        "the page, then the backfilled page, and no third"
    );
    let delivery = delivery(&rig);
    assert_eq!((delivery.gaps, delivery.backfills), (2, 1));
    assert_eq!(residuals(&rig, "gaps").await, 0);
    assert!(rig.acknowledged(&session).await.is_empty());
    // Control: the next tail delivers both entries.
    rig.turn(&session, "t2", &admission).await.expect("served");
    assert_eq!(residuals(&rig, "gaps").await, 2);
}

/// A session with learned history whose delivery failed, left owing one
/// entry.
async fn owing(rig: &Rig, session: &SessionId, project_name: &str) {
    rig.learner
        .script(ApplyScript::Refuse(LearnerError::Unavailable(
            "down".into(),
        )));
    rig.turn(session, "t1", &admission(project_name, Some(shadow())))
        .await
        .expect("served");
    assert!(rig.acknowledged(session).await.is_empty());
}

/// A session whose project writes no learner block any more still delivers,
/// under `UNCONFIGURED_APPLY_TIMEOUT_MS`: long enough for an apply that
/// answers in tens of milliseconds, and not forever. The stalls are literals,
/// so a change to the constant cannot move both sides of the test.
#[tokio::test]
async fn a_session_without_a_learner_block_delivers_under_the_unconfigured_timeout() {
    let rig = Rig::new(RigConfig::default());
    let quick = SessionId::new("bare/ada/quick");
    let slow = SessionId::new("bare/ada/slow");
    owing(&rig, &quick, "bare").await;
    owing(&rig, &slow, "bare").await;
    let bare = admission("bare", None);
    assert_eq!(bare.learner_apply_timeout_ms, None);

    rig.learner
        .script(ApplyScript::LandThenStall(Duration::from_millis(40)));
    rig.turn(&quick, "t2", &bare).await.expect("served");
    assert_eq!(
        rig.acknowledged(&quick).await.len(),
        1,
        "an apply that answers in 40 ms is acknowledged"
    );
    assert_eq!(delivery(&rig).timed_out, 0);

    rig.learner
        .script(ApplyScript::LandThenStall(Duration::from_millis(1_500)));
    rig.turn(&slow, "t2", &bare).await.expect("served");
    assert!(
        rig.acknowledged(&slow).await.is_empty(),
        "an apply that answers after 1.5 s is left pending"
    );
    assert_eq!(delivery(&rig).timed_out, 1);
}

/// An `off` block's written apply timeout is the one its sessions deliver
/// under: an apply that answers in 600 ms lands under a written 2 s, where
/// the unconfigured timeout would have given up.
#[tokio::test]
async fn an_off_block_delivers_under_its_written_apply_timeout() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("quiet/ada/s");
    owing(&rig, &session, "quiet").await;
    // What an `off` block that writes `apply_timeout_ms` resolves to.
    let off = Admission {
        learner_apply_timeout_ms: Some(2_000),
        ..admission("quiet", None)
    };
    rig.learner
        .script(ApplyScript::LandThenStall(Duration::from_millis(600)));
    rig.turn(&session, "t2", &off).await.expect("served");
    assert_eq!(rig.acknowledged(&session).await.len(), 1);
    assert_eq!(delivery(&rig).timed_out, 0);
}

/// `TurnResult.last_seq` is the session's sequence after the tail's
/// `LearningApplied` append: the cursor a client resumes from covers the
/// whole turn.
#[tokio::test]
async fn a_learner_turns_last_seq_includes_its_acknowledgement() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("cursor/ada/s");
    let result = rig
        .turn(&session, "t1", &admission("cursor", Some(shadow())))
        .await
        .expect("served");
    let events = rig.events(&session).await;
    let last = events.last().expect("a log");
    assert!(
        matches!(last.kind, SessionEventKind::LearningApplied { .. }),
        "the acknowledgement is the turn's last event: {:?}",
        last.kind
    );
    assert_eq!(result.last_seq, last.seq);
}

/// `refuse` fails a live turn on a store outage; `serve_rules` serves it.
#[tokio::test]
async fn a_refuse_project_fails_the_turn_on_a_store_outage_and_a_serve_rules_project_does_not() {
    use roundhouse_core::routing::learn::{OnInfeasible, ReadFailure, StoreRead, Unmet};
    let rig = Rig::new(RigConfig::default());
    rig.learner.fail_reads(true);
    let live = |on_infeasible| LearnerTerms {
        on_infeasible,
        ..terms(LearnerMode::Live)
    };
    let serving = SessionId::new("outage/ada/serve");
    let served = rig
        .turn(
            &serving,
            "t1",
            &admission("outage", Some(live(OnInfeasible::ServeRules))),
        )
        .await
        .expect("serve_rules serves through an outage");
    assert_eq!(served.decision.expect("routed").target, large());
    assert_eq!(
        rig.last_learned(&serving).await.view,
        StoreRead::Unavailable {
            reason: ReadFailure::StoreUnavailable
        }
    );
    let refusing = SessionId::new("outage/ada/refuse");
    match rig
        .turn(
            &refusing,
            "t1",
            &admission("outage", Some(live(OnInfeasible::Refuse))),
        )
        .await
    {
        Err(roundhouse_server::EngineError::LearnerRefused { unmet }) => {
            assert_eq!(unmet, vec![Unmet::StoreUnavailable])
        }
        other => panic!(
            "expected a learner refusal, got {:?}",
            other.map(|r| r.decision)
        ),
    }
}
