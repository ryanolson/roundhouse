// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The fold's validation of recorded review coverage, over crafted logs.
//!
//! The runtime cannot produce most of these orderings: a review is captured
//! and committed within one turn, under the lease. These are guards against
//! logs that were corrupted, forged, replayed twice, or written by a build with
//! a bug. Each rejected update must move nothing, and each accepted one must
//! name exactly the decisions the log holds.

mod review_support;

use roundhouse_core::event::{
    ControlRecord, NotRunReason, PlaceboTiming, SessionEvent, SessionEventKind, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, SideCallId, ValidationId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::CacheLedger;
use roundhouse_core::session::{MAX_REVIEW_TURNS, SessionState};
use roundhouse_core::store::doubles::ReplayLog;
use roundhouse_core::validate::{
    Arm, CoverageGap, IntervalLabel, IntervalReview, ObjectiveVersion, REVIEW_RULE_REVISION,
    ReviewedDecision, SteerAction, TriggerRecord, Verdict,
};

use review_support::*;

fn verdict(raw: &str) -> Verdict {
    Verdict::parse(raw).expect("a fixture verdict")
}

/// A judged validation carrying `interval`, as the engine would commit it.
fn judged(interval: Option<IntervalReview>, raw: &str) -> ControlRecord {
    let mut record = ControlRecord::default();
    record.validation_decided(
        ValidationId::generate(),
        TriggerRecord::new(1, 0, Vec::new()),
        Arm::Shadow,
        ValidationOutcome::Judged {
            side_call_id: SideCallId::generate(),
            verdict: verdict(raw),
            action: SteerAction::Continue,
            interval: interval.map(Box::new),
        },
    );
    record
}

fn not_run(reason: NotRunReason) -> ControlRecord {
    let mut record = ControlRecord::default();
    record.validation_decided(
        ValidationId::generate(),
        TriggerRecord::new(1, 0, Vec::new()),
        Arm::Shadow,
        ValidationOutcome::NotRun { reason },
    );
    record
}

/// A turn recorded in full, returning its decision as a review would name it.
async fn turn(log: &mut Log, name: &str) -> ReviewedDecision {
    let response = log.begin(name, vec![Item::user_text(name)]).await;
    let routed_seq = log
        .route(&response, Some(ObjectiveVersion::Undeclared))
        .await;
    let turn_index = log.session.turn_index() - 1;
    log.complete(&response, "answer").await;
    ReviewedDecision {
        routed_seq,
        turn_index,
        response_id: response,
    }
}

fn review(
    after: u64,
    through: u64,
    decisions: &[ReviewedDecision],
    label: IntervalLabel,
) -> IntervalReview {
    IntervalReview {
        rule_revision: REVIEW_RULE_REVISION,
        after_seq: after,
        through_seq: through,
        decisions: decisions.to_vec(),
        gaps: Vec::new(),
        prompt_digest: "0".repeat(64),
        label,
    }
}

/// Commit `record` and return the state a fresh replay reaches.
async fn commit(log: &mut Log, record: ControlRecord) -> SessionState {
    log.session.record_control(record).await.unwrap();
    log.replay().await
}

#[tokio::test]
async fn duplicate_overlapping_and_gapped_reviews_are_rejected() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let d0 = turn(&mut log, "t0").await;
    let d1 = turn(&mut log, "t1").await;
    let through = log.state().last_seq;
    let first = review(
        0,
        through,
        &[d0.clone(), d1.clone()],
        IntervalLabel::Positive,
    );

    let state = commit(&mut log, judged(Some(first.clone()), ON_TRACK)).await;
    assert_eq!(
        (
            state.review_checkpoint(),
            state.accepted_reviews(),
            state.rejected_reviews()
        ),
        (through, 1, 0)
    );

    // Delivered twice: the second copy starts at a checkpoint that has moved.
    let state = commit(&mut log, judged(Some(first.clone()), ON_TRACK)).await;
    assert_eq!(
        (
            state.review_checkpoint(),
            state.accepted_reviews(),
            state.rejected_reviews()
        ),
        (through, 1, 1)
    );
    assert_eq!(state.review_outcomes().len(), 1, "one label per interval");

    let d2 = turn(&mut log, "t2").await;
    let later = log.state().last_seq;
    // Overlapping: re-covers the reviewed interval along with the new decision.
    let overlapping = review(0, later, &[d0, d1, d2.clone()], IntervalLabel::Positive);
    let state = commit(&mut log, judged(Some(overlapping), ON_TRACK)).await;
    assert_eq!(
        (state.review_checkpoint(), state.rejected_reviews()),
        (through, 2)
    );
    // Gapped: starts after the checkpoint, skipping an unexplained span.
    let gapped = review(
        through + 1,
        later,
        std::slice::from_ref(&d2),
        IntervalLabel::Positive,
    );
    let state = commit(&mut log, judged(Some(gapped), ON_TRACK)).await;
    assert_eq!(
        (state.review_checkpoint(), state.rejected_reviews()),
        (through, 3)
    );
    assert_eq!(
        state.pending_review_decisions().collect::<Vec<_>>(),
        vec![d2.routed_seq]
    );

    // The control: the contiguous update is accepted.
    let next = review(
        through,
        later,
        std::slice::from_ref(&d2),
        IntervalLabel::Negative,
    );
    let state = commit(&mut log, judged(Some(next), OFF_TRACK)).await;
    assert_eq!(
        (state.review_checkpoint(), state.accepted_reviews()),
        (later, 2)
    );
    let outcome = state.review_outcomes().last().unwrap();
    assert_eq!(
        (outcome.decisions.clone(), outcome.label),
        (vec![d2.routed_seq], IntervalLabel::Negative)
    );
}

#[tokio::test]
async fn fabricated_membership_is_rejected() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let d0 = turn(&mut log, "t0").await;
    let d1 = turn(&mut log, "t1").await;
    let through = log.state().last_seq;
    let fabricated = ReviewedDecision {
        routed_seq: d1.routed_seq + 1,
        ..d1.clone()
    };
    let wrong_turn = ReviewedDecision {
        turn_index: 7,
        ..d1.clone()
    };
    let wrong_response = ReviewedDecision {
        response_id: ResponseId::new("resp_forged"),
        ..d1.clone()
    };
    let forgeries: Vec<(&str, IntervalReview)> = vec![
        (
            "a decision omitted",
            review(
                0,
                through,
                std::slice::from_ref(&d0),
                IntervalLabel::Positive,
            ),
        ),
        (
            "a decision invented",
            review(
                0,
                through,
                &[d0.clone(), d1.clone(), fabricated],
                IntervalLabel::Positive,
            ),
        ),
        (
            "a wrong turn index",
            review(
                0,
                through,
                &[d0.clone(), wrong_turn],
                IntervalLabel::Positive,
            ),
        ),
        (
            "a wrong response",
            review(
                0,
                through,
                &[d0.clone(), wrong_response],
                IntervalLabel::Positive,
            ),
        ),
        (
            "out of order",
            review(
                0,
                through,
                &[d1.clone(), d0.clone()],
                IntervalLabel::Positive,
            ),
        ),
        (
            "repeated",
            review(
                0,
                through,
                &[d0.clone(), d0.clone(), d1.clone()],
                IntervalLabel::Positive,
            ),
        ),
        (
            "unknown with invented membership",
            IntervalReview {
                gaps: vec![CoverageGap::Oversized],
                ..review(
                    0,
                    through,
                    std::slice::from_ref(&d0),
                    IntervalLabel::Unknown,
                )
            },
        ),
    ];
    for (n, (why, forged)) in forgeries.into_iter().enumerate() {
        let state = commit(&mut log, judged(Some(forged), ON_TRACK)).await;
        assert_eq!(state.review_checkpoint(), 0, "{why}");
        assert_eq!(state.rejected_reviews(), n as u64 + 1, "{why}");
    }
    // A capture cannot reach the event that records it.
    let own_seq = log.state().last_seq + 1;
    let state = commit(
        &mut log,
        judged(
            Some(review(
                0,
                own_seq,
                &[d0.clone(), d1.clone()],
                IntervalLabel::Positive,
            )),
            ON_TRACK,
        ),
    )
    .await;
    assert_eq!(
        (state.review_checkpoint(), state.rejected_reviews()),
        (0, 8)
    );
    let through = log.state().last_seq;
    let honest = review(
        0,
        through,
        &[d0.clone(), d1.clone()],
        IntervalLabel::Positive,
    );
    let state = commit(&mut log, judged(Some(honest), ON_TRACK)).await;
    assert_eq!(
        state.review_checkpoint(),
        through,
        "the control is accepted"
    );
    assert_eq!(state.accepted_reviews(), 1);
}

/// A review whose capture falls before a later decision leaves that decision,
/// and its turn, for the next review.
#[tokio::test]
async fn a_late_review_leaves_genuinely_later_decisions_for_the_next() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let d0 = turn(&mut log, "t0").await;
    let response = log.begin("t1", vec![Item::user_text("t1")]).await;
    let captured = log.state().last_seq;
    let d1 = log
        .route(&response, Some(ObjectiveVersion::Undeclared))
        .await;
    log.complete(&response, "answer").await;

    let state = commit(
        &mut log,
        judged(
            Some(review(
                0,
                captured,
                std::slice::from_ref(&d0),
                IntervalLabel::Positive,
            )),
            ON_TRACK,
        ),
    )
    .await;
    assert_eq!(state.review_checkpoint(), captured);
    assert_eq!(
        state.pending_review_decisions().collect::<Vec<_>>(),
        vec![d1]
    );

    let through = log.state().last_seq;
    let later = ReviewedDecision {
        routed_seq: d1,
        turn_index: 1,
        response_id: response,
    };
    let state = commit(
        &mut log,
        judged(
            Some(review(captured, through, &[later], IntervalLabel::Positive)),
            ON_TRACK,
        ),
    )
    .await;
    assert_eq!(state.accepted_reviews(), 2);
    assert_eq!(state.review_outcomes()[1].decisions, vec![d1]);
}

/// A judged record written before coverage existed is a checkpoint: a later
/// review cannot relabel what it saw. It supplies no label itself.
#[tokio::test]
async fn a_historical_judgement_checkpoints_without_a_label() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    turn(&mut log, "t0").await;
    turn(&mut log, "t1").await;
    let state = commit(&mut log, judged(None, ON_TRACK)).await;
    let historical = log.state().last_seq;
    assert_eq!(state.review_checkpoint(), historical);
    assert_eq!(state.accepted_reviews(), 0);
    assert!(
        state.review_outcomes().is_empty(),
        "no label for an uncovered review"
    );
    assert_eq!(state.pending_review_decisions().count(), 0);

    let d2 = turn(&mut log, "t2").await;
    let through = log.state().last_seq;
    let state = commit(
        &mut log,
        judged(
            Some(review(
                historical,
                through,
                std::slice::from_ref(&d2),
                IntervalLabel::Positive,
            )),
            ON_TRACK,
        ),
    )
    .await;
    assert_eq!(state.review_outcomes()[0].decisions, vec![d2.routed_seq]);
}

#[tokio::test]
async fn outcomes_that_asked_nobody_do_not_move_the_checkpoint() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let d0 = turn(&mut log, "t0").await;
    for reason in [
        NotRunReason::BudgetRefused,
        NotRunReason::ReviewBudgetSpent,
        NotRunReason::JudgeUnavailable,
        NotRunReason::JudgeFailed,
        NotRunReason::VerdictUnparseable,
        NotRunReason::PlaceboArm {
            timing: PlaceboTiming::Quiet,
        },
        NotRunReason::PlaceboArm {
            timing: PlaceboTiming::Intervened,
        },
        NotRunReason::PlaceboArm {
            timing: PlaceboTiming::Withheld,
        },
    ] {
        let state = commit(&mut log, not_run(reason)).await;
        assert_eq!(state.review_checkpoint(), 0, "{reason:?}");
        assert_eq!(
            state.pending_review_decisions().collect::<Vec<_>>(),
            vec![d0.routed_seq],
            "{reason:?}"
        );
    }
    let through = log.state().last_seq;
    let state = commit(
        &mut log,
        judged(
            Some(review(
                0,
                through,
                std::slice::from_ref(&d0),
                IntervalLabel::Positive,
            )),
            ON_TRACK,
        ),
    )
    .await;
    assert_eq!(state.review_checkpoint(), through);
}

/// Beyond the tracking bound the fold cannot vouch for membership, so a claim
/// of complete coverage is refused. An unknown review closes the interval, and
/// the next interval is complete again.
#[tokio::test]
async fn an_overflowed_interval_accepts_only_an_unknown_review() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    for n in 0..MAX_REVIEW_TURNS + 3 {
        turn(&mut log, &format!("t{n}")).await;
    }
    let tracked: Vec<u64> = log.replay().await.pending_review_decisions().collect();
    let through = log.state().last_seq;
    let decisions: Vec<ReviewedDecision> = log
        .events()
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed {
                response_id,
                decision,
            } if tracked.contains(&event.seq) => Some(ReviewedDecision {
                routed_seq: event.seq,
                turn_index: decision
                    .selection
                    .map(|s| s.features.turn_index)
                    .unwrap_or_default(),
                response_id,
            }),
            _ => None,
        })
        .collect();
    let claimed_complete = review(0, through, &decisions, IntervalLabel::Positive);
    let state = commit(&mut log, judged(Some(claimed_complete), ON_TRACK)).await;
    assert_eq!(
        (state.review_checkpoint(), state.rejected_reviews()),
        (0, 1),
        "the omitted prefix is not forgotten"
    );

    let unknown = IntervalReview {
        gaps: vec![CoverageGap::MetadataOverflow],
        ..review(0, through, &[], IntervalLabel::Unknown)
    };
    let state = commit(&mut log, judged(Some(unknown), ON_TRACK)).await;
    assert_eq!(state.review_checkpoint(), through);
    assert_eq!(state.review_outcomes()[0].label, IntervalLabel::Unknown);

    let fresh = turn(&mut log, "fresh").await;
    let later = log.state().last_seq;
    let state = commit(
        &mut log,
        judged(
            Some(review(through, later, &[fresh], IntervalLabel::Positive)),
            ON_TRACK,
        ),
    )
    .await;
    assert_eq!(state.accepted_reviews(), 2);
    assert_eq!(state.review_outcomes()[1].label, IntervalLabel::Positive);
}

/// A decision without a terminal event has no durable output, so complete
/// coverage of it cannot be claimed.
#[tokio::test]
async fn an_unterminated_decision_cannot_be_claimed_complete() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let lost = log.begin("t0", vec![Item::user_text("t0")]).await;
    let d0 = ReviewedDecision {
        routed_seq: log.route(&lost, Some(ObjectiveVersion::Undeclared)).await,
        turn_index: 0,
        response_id: lost,
    };
    let d1 = turn(&mut log, "t0").await;
    let through = log.state().last_seq;
    let claimed = review(
        0,
        through,
        &[d0.clone(), d1.clone()],
        IntervalLabel::Positive,
    );
    let state = commit(&mut log, judged(Some(claimed), ON_TRACK)).await;
    assert_eq!(state.rejected_reviews(), 1);
    let honest = IntervalReview {
        gaps: vec![CoverageGap::UnterminatedTurn],
        ..review(0, through, &[d0, d1], IntervalLabel::Unknown)
    };
    let state = commit(&mut log, judged(Some(honest), ON_TRACK)).await;
    assert_eq!(state.review_checkpoint(), through);
}

/// A label must follow from its gaps and verdict under the current rule. A
/// review written under another revision keeps the label it was written with.
#[tokio::test]
async fn labels_must_agree_with_the_current_rule_and_older_ones_are_kept() {
    // (why, label, gaps, verdict, revision, accepted)
    type Case<'a> = (&'a str, IntervalLabel, Vec<CoverageGap>, &'a str, u32, bool);
    let cases: Vec<Case> = vec![
        (
            "positive over a gap",
            IntervalLabel::Positive,
            vec![CoverageGap::Oversized],
            ON_TRACK,
            REVIEW_RULE_REVISION,
            false,
        ),
        (
            "positive for an off-track verdict",
            IntervalLabel::Positive,
            vec![],
            OFF_TRACK,
            REVIEW_RULE_REVISION,
            false,
        ),
        (
            "negative for an on-track verdict",
            IntervalLabel::Negative,
            vec![],
            ON_TRACK,
            REVIEW_RULE_REVISION,
            false,
        ),
        (
            "positive despite missing context",
            IntervalLabel::Positive,
            vec![],
            MISSING_CONTEXT,
            REVIEW_RULE_REVISION,
            false,
        ),
        (
            "unknown for a complete on-track verdict",
            IntervalLabel::Unknown,
            vec![],
            ON_TRACK,
            REVIEW_RULE_REVISION,
            false,
        ),
        (
            "an older rule's label is kept",
            IntervalLabel::Positive,
            vec![],
            OFF_TRACK,
            REVIEW_RULE_REVISION - 1,
            true,
        ),
        (
            "no rule may label a gap",
            IntervalLabel::Negative,
            vec![CoverageGap::Oversized],
            OFF_TRACK,
            REVIEW_RULE_REVISION - 1,
            false,
        ),
    ];
    for (why, label, gaps, raw, revision, accepted) in cases {
        let mut log = Log::enrolled(Some(Arm::Shadow)).await;
        let d0 = turn(&mut log, "t0").await;
        let through = log.state().last_seq;
        let written = IntervalReview {
            rule_revision: revision,
            gaps,
            ..review(0, through, &[d0], label)
        };
        let state = commit(&mut log, judged(Some(written), raw)).await;
        assert_eq!(state.accepted_reviews() == 1, accepted, "{why}");
        if accepted {
            assert_eq!(state.review_outcomes()[0].label, label, "{why}");
            assert_eq!(state.review_outcomes()[0].rule_revision, revision, "{why}");
        }
    }
}

/// An unenrolled or placebo session tracks no interval at all.
#[tokio::test]
async fn sessions_that_never_consult_a_judge_track_nothing() {
    for arm in [None, Some(Arm::Placebo)] {
        let mut log = Log::enrolled(arm).await;
        turn(&mut log, "t0").await;
        assert_eq!(
            log.replay().await.pending_review_decisions().count(),
            0,
            "{arm:?}"
        );
    }
    let mut log = Log::enrolled(Some(Arm::Live)).await;
    let d0 = turn(&mut log, "t0").await;
    assert_eq!(
        log.replay()
            .await
            .pending_review_decisions()
            .collect::<Vec<_>>(),
        vec![d0.routed_seq]
    );
}

/// A replay over the serialized log, a prefix of it, and the live projection
/// reach the same answer.
#[tokio::test]
async fn replay_is_deterministic_across_serialization_and_prefixes() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let d0 = turn(&mut log, "t0").await;
    let first_through = log.state().last_seq;
    log.session
        .record_control(judged(
            Some(review(
                0,
                first_through,
                std::slice::from_ref(&d0),
                IntervalLabel::Positive,
            )),
            ON_TRACK,
        ))
        .await
        .unwrap();
    let after_first = log.state().last_seq;
    // A duplicate and a forged review, both rejected.
    log.session
        .record_control(judged(
            Some(review(
                0,
                first_through,
                std::slice::from_ref(&d0),
                IntervalLabel::Positive,
            )),
            ON_TRACK,
        ))
        .await
        .unwrap();
    let d1 = turn(&mut log, "t1").await;
    let through = log.state().last_seq;
    log.session
        .record_control(judged(
            Some(review(
                first_through,
                through,
                std::slice::from_ref(&d1),
                IntervalLabel::Negative,
            )),
            OFF_TRACK,
        ))
        .await
        .unwrap();

    let live = log.state();
    let events = log.events().await;
    let wire: Vec<SessionEvent> = events
        .iter()
        .map(|event| serde_json::from_str(&serde_json::to_string(event).unwrap()).unwrap())
        .collect();
    let replayed = project(wire.clone()).await;
    for state in [&replayed, &log.replay().await] {
        assert_eq!(state.review_checkpoint(), live.review_checkpoint());
        assert_eq!(state.accepted_reviews(), live.accepted_reviews());
        assert_eq!(state.rejected_reviews(), live.rejected_reviews());
        assert_eq!(state.review_outcomes(), live.review_outcomes());
    }
    assert_eq!((live.accepted_reviews(), live.rejected_reviews()), (2, 1));

    let prefix = project(
        wire.into_iter()
            .filter(|event| event.seq <= after_first)
            .collect(),
    )
    .await;
    assert_eq!(prefix.review_checkpoint(), first_through);
    assert_eq!(prefix.review_outcomes().len(), 1);
}

async fn project(events: Vec<SessionEvent>) -> SessionState {
    SessionState::project(
        &ReplayLog::new(events),
        &SessionId::new("acme/ada/review"),
        CacheLedger::new(),
        None,
    )
    .await
    .unwrap()
}
