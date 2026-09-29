// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The learning cursor, the `LearningApplied` event, replay, and the
//! automatic source marks (milestone M5 of
//! `agent-docs/PLAN-online-routing-learner.md`, draft sections 11.5 and
//! 11.7). The entries themselves are in `learning_entries.rs`.

mod learning_support;

use std::num::NonZeroUsize;
use std::sync::Arc;

use roundhouse_core::classify::{TierChoice, TurnComplexity};
use roundhouse_core::control::{Principal, ProjectId};
use roundhouse_core::event::{
    ControlRecord, NotRunReason, SessionEvent, SessionEventKind, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId, ValidationId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::CacheLedger;
use roundhouse_core::session::{LEARNING_PAGE, Session, SessionState, learning_mark};
use roundhouse_core::store::doubles::ReplayLog;
use roundhouse_core::store::{LearningMark, MemoryStore, SessionStore};
use roundhouse_core::validate::{Arm, TriggerRecord};

use learning_support::*;

/// A learned turn, then `count` validations that each produce an entry.
fn many(count: usize) -> Script {
    let mut script = Script::new();
    script.turn(Spec::new().decision());
    for _ in 0..count {
        script.not_run();
    }
    script
}

/// Every entry-producing sequence of `script`, in order: its completions and
/// its validations, the only kinds [`many`] writes after learned evidence.
fn produced(script: &Script) -> Vec<u64> {
    script
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                SessionEventKind::ResponseCompleted { .. }
                    | SessionEventKind::ValidationDecided { .. }
            )
        })
        .map(|event| event.seq)
        .collect()
}

// ---- Cursor --------------------------------------------------------------------

/// **The page is a prefix, never a sample.** Past the bound, entries are
/// counted and not held, and no later entry enters the page while any is
/// counted, so the order the page is delivered in never skips one.
#[tokio::test]
async fn a_full_page_counts_beyond_and_holds_no_later_entry() {
    let script = many(LEARNING_PAGE + 5);
    let all = produced(&script);
    let state = script.state().await;

    let held: Vec<u64> = state
        .learning_page()
        .iter()
        .map(|entry| entry.seq)
        .collect();
    assert_eq!(held, all[..LEARNING_PAGE], "the first entries, in order");
    assert_eq!(state.learning_beyond(), 6, "{} entries in all", all.len());

    // The store confirms the whole page: nothing held, the rest still owed.
    let mut script = script;
    script.applied(*held.last().unwrap());
    let state = script.state().await;
    assert!(state.learning_page().is_empty());
    assert!(
        state.learning_beyond() > 0,
        "entries past the page are still owed until a backfill finds them"
    );
}

#[tokio::test]
async fn backfill_from_a_seq_refills_the_page_in_order() {
    let script = many(LEARNING_PAGE + 5);
    let all = produced(&script);
    let from = all[LEARNING_PAGE - 1];

    let state = script.backfill(from).await;
    let page = state.learning_page();
    let held: Vec<u64> = page.iter().map(|entry| entry.seq).collect();
    assert_eq!(held, all[LEARNING_PAGE..], "the entries above the floor");
    assert_eq!(page[0].prev_seq, from, "the chain runs through the floor");
    assert_eq!(state.learning_beyond(), 0);
}

/// **The learner store's watermark outranks the log's hint.** When the store
/// lost writes the log already acknowledged (draft 11.6), or an audit finds a
/// watermark below the mark (11.7), the backfill from that watermark must hold
/// the entries between it and the hint, or the store answers `ChainGap` with
/// the same watermark forever. The live fold on the same log is the control:
/// it still drops what `LearningApplied` confirmed.
#[tokio::test]
async fn a_backfill_below_a_stale_hint_refills_from_the_floor() {
    let mut script = many(8);
    let all = produced(&script);
    script.applied(all[5]);

    let live = script.state().await;
    let held: Vec<u64> = live.learning_page().iter().map(|entry| entry.seq).collect();
    assert_eq!(held, all[6..], "the live fold prunes at the hint");

    let state = script.backfill(all[1]).await;
    let page = state.learning_page();
    let held: Vec<u64> = page.iter().map(|entry| entry.seq).collect();
    assert_eq!(held, all[2..], "every entry above the store's watermark");
    assert_eq!(page[0].prev_seq, all[1], "the chain runs through the floor");
    assert_eq!(state.learning_beyond(), 0);
    assert_eq!(state.learning_hint(), all[5], "the hint is still recorded");
}

/// **A floor backfill that overflows its page still counts what lies beyond
/// it**, even after an acknowledgement of the newest entry. A reset of
/// `beyond` on `LearningApplied` would stop recovery a page early.
#[tokio::test]
async fn a_floor_backfill_past_a_page_counts_beyond_through_an_acknowledgement() {
    let mut script = many(LEARNING_PAGE + 5);
    let all = produced(&script);
    script.applied(*all.last().expect("entries were produced"));

    let state = script.backfill(all[1]).await;
    let held: Vec<u64> = state
        .learning_page()
        .iter()
        .map(|entry| entry.seq)
        .collect();
    assert_eq!(
        held,
        all[2..2 + LEARNING_PAGE],
        "one full page above the floor"
    );
    let past_the_page = (all.len() - 2 - LEARNING_PAGE) as u64;
    assert!(
        past_the_page > 0,
        "the premise: the backfill overflows its page"
    );
    assert_eq!(
        state.learning_beyond(),
        past_the_page,
        "exactly the entries past the page"
    );
}

#[tokio::test]
async fn learning_applied_moves_the_hint() {
    let mut script = many(4);
    let all = produced(&script);
    script.applied(all[2]);
    // A delayed, older acknowledgement never moves the hint back.
    script.applied(all[0]);

    let state = script.state().await;
    assert_eq!(state.learning_hint(), all[2]);
    let held: Vec<u64> = state
        .learning_page()
        .iter()
        .map(|entry| entry.seq)
        .collect();
    assert_eq!(held, all[3..], "the page drops what the store confirmed");
}

#[test]
fn learning_applied_has_no_response_id_and_is_not_terminal() {
    let event = SessionEvent {
        seq: 9,
        session_id: SessionId::new("s"),
        at_ms: 0,
        kind: SessionEventKind::LearningApplied { through_seq: 7 },
    };
    assert_eq!(event.response_id(), None);
    assert!(!event.is_terminal());
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "learning_applied");
    assert_eq!(wire["through_seq"], 7);
}

/// A successor that replays the durable log, or its serialized copy, reaches
/// the entries and the cursor the writer's own fold holds.
#[tokio::test]
async fn replay_rebuilds_the_same_entries_and_cursor() {
    let (store, id, mut session) = open("acme/ada/replay").await;
    let first = learned_turn(&mut session, "t0").await;
    let second = learned_turn(&mut session, "t1").await;
    session
        .record_control(not_run())
        .await
        .expect("a validation");
    session.record_learning_applied(first).await.unwrap();
    let live = session.state();
    assert!(!live.learning_page().is_empty(), "the fixture owes entries");
    assert_eq!(live.learning_hint(), first);

    let events = store.read_events(&id, 0, 10_000).await.unwrap();
    let wire = serde_json::to_string(&events).unwrap();
    let decoded: Vec<SessionEvent> = serde_json::from_str(&wire).unwrap();
    for replayed in [
        SessionState::project(store.as_ref(), &id, CacheLedger::new(), None)
            .await
            .unwrap(),
        SessionState::project(&ReplayLog::new(decoded), &id, CacheLedger::new(), None)
            .await
            .unwrap(),
    ] {
        assert_eq!(replayed.learning_page(), live.learning_page());
        assert_eq!(replayed.learning_beyond(), live.learning_beyond());
        assert_eq!(replayed.learning_hint(), live.learning_hint());
        assert_eq!(replayed.learning_causes(), live.learning_causes());
    }
    assert!(second > first);
}

// ---- Marks -----------------------------------------------------------------------

fn completed(response: &str) -> SessionEventKind {
    SessionEventKind::ResponseCompleted {
        response_id: ResponseId::new(response),
        usage: unmeasured(),
        provider_reported_cost_usd: None,
        stop_reason: None,
    }
}

fn routed(response: &str, decision: roundhouse_core::routing::DecisionRecord) -> SessionEventKind {
    SessionEventKind::Routed {
        response_id: ResponseId::new(response),
        decision,
    }
}

fn acme() -> ProjectId {
    ProjectId::from("acme")
}

#[tokio::test]
async fn learning_mark_marks_every_entry_producing_append_after_the_first_learned_routed() {
    let mut script = Script::new();
    let turn = script.turn(Spec::new().decision());
    let learned = script.state().await;
    let fresh = Script::new().state().await;
    let answer = SessionEventKind::ClassificationRecorded {
        record: answer(
            &turn,
            classification(TurnComplexity::Routine, Some(TierChoice::Capable)),
        ),
    };

    // Each entry-producing kind, alone after learned evidence.
    for kind in [
        completed("r9"),
        answer.clone(),
        SessionEventKind::ResponseIncomplete {
            response_id: ResponseId::new("r9"),
            reason: roundhouse_core::event::IncompleteReason::UpstreamError,
            usage: unmeasured(),
            terminal_attempt: None,
        },
        not_run().into_kinds().remove(0),
    ] {
        assert_eq!(
            learning_mark(&learned, std::slice::from_ref(&kind)),
            Some(LearningMark::new(0, acme())),
            "{kind:?}"
        );
    }
    // The newest entry-producing event of the batch.
    let batch = [
        not_run().into_kinds().remove(0),
        SessionEventKind::ItemAppended {
            item: Item::user_text("done"),
        },
        completed("r9"),
        SessionEventKind::LearningApplied { through_seq: 1 },
    ];
    assert_eq!(
        learning_mark(&learned, &batch),
        Some(LearningMark::new(2, acme()))
    );
    // Learned evidence that arrives inside the batch counts for what follows.
    let batch = [routed("r9", Spec::new().decision()), completed("r9")];
    assert_eq!(
        learning_mark(&fresh, &batch),
        Some(LearningMark::new(1, acme()))
    );
}

#[tokio::test]
async fn learning_mark_marks_nothing_before_learned_evidence() {
    let fresh = Script::new().state().await;
    let mut script = Script::new();
    script.turn(unlearned(opus()));
    let staged = script.state().await;
    let mut anonymous = Script::with(None, Some(Arm::Shadow));
    anonymous.turn(Spec::new().decision());
    let anonymous = anonymous.state().await;

    for state in [&fresh, &staged] {
        assert_eq!(learning_mark(state, &[completed("r9")]), None);
        // The completion precedes the learned `Routed` in the batch.
        let batch = [completed("r9"), routed("r10", Spec::new().decision())];
        assert_eq!(learning_mark(state, &batch), None);
        assert_eq!(
            learning_mark(state, &[routed("r9", unlearned(opus())), completed("r9")]),
            None
        );
    }
    assert_eq!(
        learning_mark(&anonymous, &[completed("r9")]),
        None,
        "a session with no principal has no project to mark under"
    );
}

/// **Every public write that can carry an entry-producing event marks the
/// session, at that event's sequence**, through `commit`. A write whose batch
/// holds several names the newest one.
#[tokio::test]
async fn commit_marks_through_every_public_write_that_can_emit_an_entry_producing_event() {
    let (store, id, mut session) = open("acme/ada/marks").await;
    assert_eq!(mark_of(&store).await, None);

    // Before learned evidence nothing is marked, even a completion.
    let response = begin(&mut session, "t0").await;
    session
        .record_routing(&response, unlearned(opus()))
        .await
        .unwrap();
    session
        .complete(&response, Some("ok"), unmeasured(), None, None)
        .await
        .unwrap();
    assert_eq!(mark_of(&store).await, None);

    // `complete`.
    let completed = learned_turn(&mut session, "t1").await;
    assert_eq!(mark_of(&store).await, Some(completed));

    // Writes that add no entry leave the mark where it is.
    let response = begin(&mut session, "t2").await;
    session
        .record_routing(&response, Spec::new().decision())
        .await
        .unwrap();
    session.append_output(&response, "hi").await.unwrap();
    session.record_learning_applied(completed).await.unwrap();
    assert_eq!(mark_of(&store).await, Some(completed));

    // `mark_incomplete`.
    session
        .mark_incomplete(
            &response,
            "partial",
            roundhouse_core::event::IncompleteReason::UpstreamError,
            unmeasured(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(mark_of(&store).await, Some(session.last_seq()));

    // `complete_with_item`: the validation and the completion ride one batch,
    // and the completion is the newer.
    let response = begin(&mut session, "t3").await;
    session
        .complete_with_item(&response, Item::user_text("steer"), unmeasured(), not_run())
        .await
        .unwrap();
    let events = store.read_events(&id, 0, 10_000).await.unwrap();
    assert!(matches!(
        events.last().unwrap().kind,
        SessionEventKind::ResponseCompleted { .. }
    ));
    assert_eq!(mark_of(&store).await, Some(session.last_seq()));

    // `record_control`.
    session.record_control(not_run()).await.unwrap();
    assert_eq!(mark_of(&store).await, Some(session.last_seq()));

    // `record_background_classification`: the last result is the newest.
    let turns: Vec<Turn> = (0..2)
        .map(|index| Turn {
            index,
            response_id: ResponseId::new(format!("x{index}")),
            routed: Vec::new(),
        })
        .collect();
    let results = turns
        .iter()
        .map(|turn| answer(turn, classification(TurnComplexity::Routine, None)))
        .collect();
    session
        .record_background_classification(results, Vec::new())
        .await
        .unwrap();
    assert_eq!(mark_of(&store).await, Some(session.last_seq()));
}

// ---- Fixtures over the real writer -----------------------------------------------

async fn open(id: &str) -> (Arc<MemoryStore>, SessionId, Session<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let id = SessionId::new(id);
    store.create_session(&id, "stage").await.unwrap();
    let mut session = Session::open(
        Arc::clone(&store),
        id.clone(),
        "n1",
        60_000,
        CacheLedger::new(),
    )
    .await
    .unwrap();
    session
        .record_created("stage", &Principal::new("acme", "ada"), Some(Arm::Shadow))
        .await
        .unwrap();
    (store, id, session)
}

async fn begin(session: &mut Session<MemoryStore>, turn: &str) -> ResponseId {
    session
        .begin_turn(TurnId::new(turn), vec![Item::user_text(turn)])
        .await
        .unwrap()
        .response_id()
        .clone()
}

/// A learned turn through the real writer; returns its completion's sequence.
async fn learned_turn(session: &mut Session<MemoryStore>, turn: &str) -> u64 {
    let response = begin(session, turn).await;
    session
        .record_routing(&response, Spec::new().decision())
        .await
        .unwrap();
    session.append_output(&response, "answer").await.unwrap();
    session
        .complete(&response, Some("answer"), measured(500), None, None)
        .await
        .unwrap();
    session.last_seq()
}

fn not_run() -> ControlRecord {
    let mut record = ControlRecord::default();
    record.validation_decided(
        ValidationId::generate(),
        TriggerRecord::new(1, 0, Vec::new()),
        Arm::Shadow,
        ValidationOutcome::NotRun {
            reason: NotRunReason::JudgeUnavailable,
        },
    );
    record
}

/// The session's mark sequence as the store's permanent index holds it.
async fn mark_of(store: &MemoryStore) -> Option<u64> {
    let page = store
        .learning_sessions(None, NonZeroUsize::new(8).unwrap())
        .await
        .unwrap();
    assert!(page.sessions.len() <= 1, "{page:?}");
    page.sessions.first().map(|marked| {
        assert_eq!(marked.project, acme());
        marked.seq
    })
}
