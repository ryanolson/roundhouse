// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract: a bounded classification window costs the window, not the history.
//!
//! [`ClassificationWindow`] already bounds what it *names* — the durable half
//! of the quadratic-log concern its own type doc states. What this file holds
//! it to is the other half: constructing one must not collect, clone or walk
//! every classification a session ever produced on the way to the newest few.
//! A hundred-turn session and a two-thousand-turn session have the same four
//! references to write down, and must pay the same to write them down.
//!
//! Measured against the real [`SessionState`] a session's own commits build,
//! through `classifications_through` — the accessor the engine actually calls
//! (`engine.rs`, `Engine::plan`) — and not a hand-built `Vec`, so a regression
//! in either the accessor or the constructor lands here.
//!
//! **Every "does not grow" claim below has a positive control beside it.** An
//! allocation counter that has silently stopped counting satisfies a bound of
//! "no more at 2,000 than at 20" perfectly, so the same instrument, over the
//! same two sessions, is also pointed at a deliberately linear collection that
//! *must* scale. If the control stops failing to be flat, the instrument is
//! broken and every other assertion here is worthless.
//!
//! Identities are fixed width (`call_000019`, `call_001999`) for the same
//! reason: a `call_19`/`call_1999` pair differs in clone size by the digits of
//! the turn count, which would put a history-shaped artifact inside a
//! measurement whose whole claim is that nothing here is history-shaped.
//!
//! A custom global allocator is the instrument, and it counts **per thread,
//! not process-wide**. `cargo test` gives every test function its own OS
//! thread by default (`--test-threads` > 1), and `#[tokio::test]` without an
//! explicit `flavor` runs on a single-threaded runtime pinned to that same
//! thread -- so a thread-local counter sees exactly one test's allocations,
//! including everything its own spawned tasks do, and nothing any
//! concurrently-running sibling test does. A process-wide counter gated by a
//! boolean/mutex is not enough: it stops two *measured regions* from
//! overlapping, but a sibling test's ordinary, unmeasured allocations
//! (building its own fixtures on its own thread) still land in the same
//! global counter for as long as any measurement anywhere is in progress,
//! silently inflating that measurement. Each `thread_local!` below uses a
//! `const` initializer specifically so the allocator itself never triggers
//! the lazily-initialized slow path malloc for its own bookkeeping.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use roundhouse_core::classify::{
    ClassificationIntent, ClassificationOutcome, ClassificationRecord, ClassificationRef,
    ClassificationWindow, ClassifierIdentity, ContextDependence, EvaluationSpend, Graded,
    ReservationRecord, SettlementAck, TAXONOMY_VERSION, TurnClassification, TurnComplexity,
    TurnIntent,
};
use roundhouse_core::control::BudgetWindow;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::routing::{CacheLedger, ProviderPricing};
use roundhouse_core::session::{Session, SessionState};
use roundhouse_core::store::{MemoryStore, SessionStore};

struct CountingAlloc;

thread_local! {
    static LOCAL_ALLOCATED: Cell<usize> = const { Cell::new(0) };
    static LOCAL_COUNTING: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LOCAL_COUNTING.with(|counting| {
            if counting.get() > 0 {
                LOCAL_ALLOCATED.with(|allocated| allocated.set(allocated.get() + layout.size()));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            LOCAL_COUNTING.with(|counting| {
                if counting.get() > 0 {
                    LOCAL_ALLOCATED.with(|allocated| {
                        allocated.set(allocated.get() + (new_size - layout.size()))
                    });
                }
            });
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// Count bytes allocated (or grown by realloc) strictly inside `body`, on
/// this thread alone. Re-entrant-safe (nested `measure` calls on one thread
/// compose correctly via the counting depth), and immune to any other test
/// running concurrently on a different thread.
fn measure<T>(body: impl FnOnce() -> T) -> (T, usize) {
    let before = LOCAL_ALLOCATED.with(|allocated| allocated.get());
    LOCAL_COUNTING.with(|counting| counting.set(counting.get() + 1));
    let result = body();
    LOCAL_COUNTING.with(|counting| counting.set(counting.get() - 1));
    let after = LOCAL_ALLOCATED.with(|allocated| allocated.get());
    (result, after.saturating_sub(before))
}

const TTL: u64 = 30_000;

fn card() -> ProviderPricing {
    ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    }
}

fn classify_identity() -> ClassifierIdentity {
    ClassifierIdentity {
        model: "jev-1.12".to_string(),
        schema: "typesafe.systemone.choice.v1".to_string(),
        taxonomy_version: TAXONOMY_VERSION,
        projection_revision: 1,
        config_revision: 1,
    }
}

fn classify_reservation() -> ReservationRecord {
    ReservationRecord {
        rate_card: card(),
        estimated_input_tokens: 100,
        expected_output_tokens: 16,
        requested_usd: 0.0002,
        hold_ttl_ms: 30_000,
        budget_limit_usd: 100.0,
        budget_window: BudgetWindow::Total,
        member_ceiling_usd: None,
        warn_at: 0.8,
    }
}

/// Fixed width at every history length this file compares, so a clone of one
/// costs the same at turn 19 as at turn 1,999. See the module note.
fn call_id(turn: u64) -> String {
    format!("call_{turn:06}")
}

fn response_id(turn: u64) -> String {
    format!("resp_{turn:06}")
}

fn classify_intent(turn: u64) -> ClassificationIntent {
    ClassificationIntent {
        call_id: ResponseId::new(call_id(turn)),
        source_turn_index: turn,
        source_response_id: ResponseId::new(response_id(turn)),
        requested_at_ms: 0,
        expires_at_ms: 30_000,
        identity: classify_identity(),
        reservation: classify_reservation(),
    }
}

fn classify_result(turn: u64) -> ClassificationRecord {
    ClassificationRecord {
        call_id: ResponseId::new(call_id(turn)),
        source_turn_index: turn,
        source_response_id: ResponseId::new(response_id(turn)),
        completed_at_ms: turn,
        outcome: ClassificationOutcome::Classified {
            classification: TurnClassification {
                taxonomy_version: TAXONOMY_VERSION,
                intent: Graded {
                    value: TurnIntent::Implement,
                    confidence: 0.8,
                },
                complexity: Graded {
                    value: TurnComplexity::Involved,
                    confidence: 0.6,
                },
                context_dependence: Graded {
                    value: ContextDependence::Recent,
                    confidence: 0.5,
                },
            },
            spend: EvaluationSpend::Unknown {
                granted_usd: 0.0,
                settled: SettlementAck::Committed,
            },
            reported_model: None,
        },
    }
}

/// A session with `count` landed, usable classifications -- built through the
/// real commit path (`record_classification_intent` + `record_classification`),
/// which is what actually populates `SessionState::classifications` in
/// production. Returns the session, its store, and the sequence its last event
/// landed at.
async fn session_with_classifications(
    count: u64,
) -> (Session<MemoryStore>, Arc<MemoryStore>, SessionId, u64) {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::generate();
    store.create_session(&sid, "affinity").await.unwrap();
    let mut session = Session::open(
        Arc::clone(&store),
        sid.clone(),
        "node-a",
        TTL,
        CacheLedger::new(),
    )
    .await
    .unwrap();
    for turn in 0..count {
        session
            .record_classification_intent(classify_intent(turn))
            .await
            .unwrap();
        session
            .record_classification(classify_result(turn))
            .await
            .unwrap();
    }
    let cutoff = session.last_seq();
    (session, store, sid, cutoff)
}

const WINDOW: usize = 4;
const SMALL: u64 = 20;
/// A hundredfold more history than [`SMALL`], for the same four references.
const LARGE: u64 = 2_000;

/// What the four named references are actually worth: the vector that holds
/// them, plus one fixed-width id string each. Doubled where it is used as a
/// ceiling, which leaves room for allocator bucket rounding without leaving
/// room for a second copy of anything.
fn named_payload_bytes(window: usize) -> usize {
    window * (std::mem::size_of::<ClassificationRef>() + call_id(0).len())
}

/// **The required behaviour: the same window costs the same, whatever is
/// behind it.**
///
/// Two sessions a hundredfold apart in history, the same configured window,
/// the same four references to write down. The construction allocates the
/// payload it returns and nothing proportional to the history it was found in
/// -- so the two measurements are the same number, not merely close.
#[tokio::test]
async fn a_bounded_window_allocates_with_its_window_and_not_with_the_history_behind_it() {
    let (small, _, _, small_cutoff) = session_with_classifications(SMALL).await;
    let (large, _, _, large_cutoff) = session_with_classifications(LARGE).await;

    let (small_window, small_bytes) = measure(|| {
        ClassificationWindow::of(
            1,
            small_cutoff,
            WINDOW,
            small.state().classifications_through(small_cutoff),
        )
    });
    let (large_window, large_bytes) = measure(|| {
        ClassificationWindow::of(
            1,
            large_cutoff,
            WINDOW,
            large.state().classifications_through(large_cutoff),
        )
    });

    eprintln!(
        "classification window allocation: count={SMALL} -> {small_bytes} bytes \
         (available={}), count={LARGE} -> {large_bytes} bytes (available={}), \
         named payload is {} bytes",
        small_window.available,
        large_window.available,
        named_payload_bytes(WINDOW)
    );

    // The window itself is unchanged by any of this: the same four newest
    // references, and an honest count of what was left out.
    assert_eq!(small_window.named.len(), WINDOW);
    assert_eq!(large_window.named.len(), WINDOW);
    assert_eq!(small_window.available, SMALL as usize);
    assert_eq!(large_window.available, LARGE as usize);

    assert_eq!(
        large_bytes, small_bytes,
        "constructing the window must cost the window and not the history \
         behind it: {SMALL} classifications allocated {small_bytes} bytes, \
         {LARGE} allocated {large_bytes}"
    );
    assert!(
        large_bytes <= named_payload_bytes(WINDOW) * 2,
        "and what it costs must be the references it returns: {large_bytes} \
         bytes for a payload worth {}",
        named_payload_bytes(WINDOW)
    );
}

/// **The positive control for the instrument, over the same two sessions.**
///
/// Deliberately linear: every reference the cutoff admits, cloned into a
/// vector. If this stops scaling, the allocation counter has stopped counting
/// and the flatness asserted above means nothing.
#[tokio::test]
async fn collecting_the_whole_admitted_history_by_hand_does_scale_with_it() {
    let (small, _, _, small_cutoff) = session_with_classifications(SMALL).await;
    let (large, _, _, large_cutoff) = session_with_classifications(LARGE).await;

    let (small_all, small_bytes) = measure(|| {
        small
            .state()
            .classifications_through(small_cutoff)
            .cloned()
            .collect::<Vec<ClassificationRef>>()
    });
    let (large_all, large_bytes) = measure(|| {
        large
            .state()
            .classifications_through(large_cutoff)
            .cloned()
            .collect::<Vec<ClassificationRef>>()
    });

    eprintln!(
        "control, collecting every admitted reference: count={SMALL} -> \
         {small_bytes} bytes, count={LARGE} -> {large_bytes} bytes"
    );

    assert_eq!(small_all.len(), SMALL as usize);
    assert_eq!(large_all.len(), LARGE as usize);
    assert!(
        large_bytes > small_bytes * 25,
        "the instrument must still see history-shaped allocation when there is \
         some: {SMALL} -> {small_bytes} bytes, {LARGE} -> {large_bytes} bytes"
    );
}

/// **The exact available count at the recorded cutoff, and the newest
/// configured references in chronological order.**
///
/// The bound is on cost, never on what the window says: a cutoff below the
/// newest results must still count exactly what had landed by it, and name the
/// newest of *those* oldest-first.
#[tokio::test]
async fn the_window_counts_what_landed_by_its_cutoff_and_names_the_newest_in_order() {
    let count = 500u64;
    let (session, _, _, cutoff) = session_with_classifications(count).await;

    let window = ClassificationWindow::of(
        1,
        cutoff,
        WINDOW,
        session.state().classifications_through(cutoff),
    );
    assert_eq!(window.available, count as usize);
    assert_eq!(window.cutoff_seq, cutoff);
    assert_eq!(
        window
            .named
            .iter()
            .map(|reference| reference.source_turn_index)
            .collect::<Vec<_>>(),
        vec![496, 497, 498, 499],
        "the newest {WINDOW}, oldest first"
    );

    // A cutoff that excludes the three newest results. Nothing above it is
    // nameable, and the count is of what landed by it and not of the session.
    let landed = session.state().classifications();
    let earlier = landed[landed.len() - 4].reference.available_seq;
    let clipped = ClassificationWindow::of(
        1,
        earlier,
        WINDOW,
        session.state().classifications_through(earlier),
    );
    assert_eq!(clipped.available, count as usize - 3);
    assert_eq!(
        clipped
            .named
            .iter()
            .map(|reference| reference.source_turn_index)
            .collect::<Vec<_>>(),
        vec![493, 494, 495, 496]
    );
    assert!(
        clipped
            .named
            .iter()
            .all(|reference| reference.available_seq <= earlier),
        "a named classification cannot have landed above the cutoff"
    );
}

/// **Empty and zero-window behaviour, both directions.**
///
/// A session with nothing to name, and a deployment configured to name
/// nothing, are different states and neither is an error.
#[tokio::test]
async fn an_empty_history_and_a_zero_window_both_name_nothing() {
    let (empty, _, _, empty_cutoff) = session_with_classifications(0).await;
    let window = ClassificationWindow::of(
        1,
        empty_cutoff,
        WINDOW,
        empty.state().classifications_through(empty_cutoff),
    );
    assert_eq!(window.available, 0);
    assert!(window.named.is_empty());

    let (session, _, _, cutoff) = session_with_classifications(50).await;
    let none = ClassificationWindow::of(
        1,
        cutoff,
        0,
        session.state().classifications_through(cutoff),
    );
    assert_eq!(none.window, 0);
    assert_eq!(
        none.available, 50,
        "a window that names nothing still counts what it left out"
    );
    assert!(none.named.is_empty());
}

/// **The precondition the cutoff search rests on, live and after replay.**
///
/// `classifications` is appended in fold order and stamped with the event's own
/// sequence, so it is ordered by `available_seq` -- which is what lets the
/// cutoff be found without reading every entry. Asserted over a real commit
/// sequence and over a successor's replay of the same log, because an
/// out-of-order fold would make the search silently wrong rather than slow.
#[tokio::test]
async fn landed_classifications_are_ordered_by_the_sequence_they_landed_at() {
    let (session, store, sid, _) = session_with_classifications(50).await;

    let ordered = |state: &SessionState| {
        state
            .classifications()
            .windows(2)
            .all(|pair| pair[0].reference.available_seq < pair[1].reference.available_seq)
    };
    assert_eq!(session.state().classifications().len(), 50);
    assert!(ordered(session.state()), "live");

    session.release().await.unwrap();
    let replayed = SessionState::project(store.as_ref(), &sid, CacheLedger::new(), None)
        .await
        .unwrap();
    assert_eq!(replayed.classifications().len(), 50);
    assert!(ordered(&replayed), "after replay");
}
