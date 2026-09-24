// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract: when its bound rejects a reviewed-turns section, the rejection
//! costs nothing in proportion to the items in that section.
//!
//! The fold bounds an interval by turns and decisions, not by items, so one
//! turn can hold any number of tool calls. The byte bound of the section is
//! the only bound on those calls. Work over every item before the bound check
//! is work that the bound does not limit.
//!
//! **Measured as a difference, because the rest of a review is not flat.**
//! The trigger and the classic brief both walk the whole session. So the total
//! allocation of a review grows with the session, whatever the section does.
//! Each test therefore measures two reviews of the same items:
//!
//! - one with the decision stamped, so that the capture reaches the section
//! - one with the decision unstamped, so that the fold records a gap and the
//!   capture does not render
//!
//! Both reviews build the same trigger evidence and the same classic brief. So
//! their difference is the cost of reaching the section.
//!
//! **Every "does not grow" claim has a positive control beside it.** A counter
//! that does not count satisfies "no more at 2,000 than at 20" perfectly. So
//! the same instrument also measures a section that renders, over the same
//! sessions. That cost must grow.
//!
//! The instrument is a per-thread counting allocator, for the reason that
//! `classification_window_allocation.rs` gives. `#[tokio::test]` runs each test
//! on its own thread. So a thread-local counter sees the allocations of one
//! test and none from a sibling test.

mod review_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::Future;

use roundhouse_core::ids::ResponseId;
use roundhouse_core::item::Item;
use roundhouse_core::validate::{Arm, CoverageGap, IntervalReview, Objective, ObjectiveVersion};

use review_support::*;

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

/// Bytes allocated, or grown by realloc, on this thread while `work` runs.
///
/// The caller builds the future before the count starts. So the count holds
/// only what polling the future allocates.
async fn measured<T>(work: impl Future<Output = T>) -> (T, usize) {
    let before = LOCAL_ALLOCATED.with(|allocated| allocated.get());
    LOCAL_COUNTING.with(|counting| counting.set(counting.get() + 1));
    let result = work.await;
    LOCAL_COUNTING.with(|counting| counting.set(counting.get() - 1));
    let after = LOCAL_ALLOCATED.with(|allocated| allocated.get());
    (result, after.saturating_sub(before))
}

const SMALL: usize = 20;
/// A hundredfold more items than [`SMALL`], in the same two turns.
const LARGE: usize = 2_000;

/// Fixed width, so an id costs the same at call 19 as at call 1,999 and no
/// item-count artifact hides inside a clone.
fn call_id(n: usize) -> String {
    format!("call_{n:06}")
}

/// One answered turn that made `calls` tool calls, and the open turn that
/// carries their results.
///
/// `stamped` decides whether the decision recorded its objective. An unstamped
/// decision gives the fold a `VersionsUnavailable` gap, so its capture stops
/// before the section.
async fn interval(calls: usize, stamped: bool) -> (Log, ResponseId) {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log
        .begin("t0", vec![Item::user_text("run every check")])
        .await;
    log.route(&first, stamped.then_some(ObjectiveVersion::Undeclared))
        .await;
    for n in 0..calls {
        log.emit(&first, Item::tool_call(call_id(n), "grep", "{}"))
            .await;
    }
    log.session
        .complete(&first, None, usage(), None, None)
        .await
        .unwrap();
    let results = (0..calls).map(|n| tool_result(&call_id(n), "ok")).collect();
    let current = log.begin("t1", results).await;
    (log, current)
}

/// What one review of that interval allocated, and the coverage it recorded.
///
/// An unmeasured review of the same state runs first, so one-time setup such
/// as tracing callsite registration lands in neither side of a difference.
async fn review_cost(calls: usize, stamped: bool, limit: usize) -> (usize, IntervalReview) {
    let (log, current) = interval(calls, stamped).await;
    let judge = ScriptedJudge::answering(&[ON_TRACK, ON_TRACK]);
    let validator = validator_over(judge, limit);
    let terms = observing();
    consider(
        &validator,
        &terms,
        log.state(),
        &current,
        Objective::Unknown,
    )
    .await;
    let (decided, bytes) = measured(consider(
        &validator,
        &terms,
        log.state(),
        &current,
        Objective::Unknown,
    ))
    .await;
    (bytes, interval_of(&decided).expect("a parsed review"))
}

/// What reaching the section cost over `calls` items under `limit`: the
/// stamped review's bytes less the unstamped review's.
async fn section_cost(calls: usize, limit: usize) -> (i64, IntervalReview) {
    let (reached, review) = review_cost(calls, true, limit).await;
    let (stopped, control) = review_cost(calls, false, limit).await;
    eprintln!(
        "{calls} calls, bound {limit}: whole review {reached} bytes, \
         baseline {stopped} bytes"
    );
    assert_eq!(
        control.gaps,
        vec![CoverageGap::VersionsUnavailable],
        "the unstamped baseline must stop before the section"
    );
    (reached as i64 - stopped as i64, review)
}

/// **The required behavior: a bound of one byte costs the same over 2,000
/// items as over 20.**
///
/// The section cannot fit, so the rejection needs nothing from its items.
#[tokio::test]
async fn a_section_its_bound_rejects_costs_nothing_proportional_to_its_items() {
    let (small, small_review) = section_cost(SMALL, 1).await;
    let (large, large_review) = section_cost(LARGE, 1).await;
    eprintln!(
        "section cost under a one-byte bound: {SMALL} calls -> {small} bytes, \
         {LARGE} calls -> {large} bytes"
    );
    assert_eq!(small_review.gaps, vec![CoverageGap::Oversized]);
    assert_eq!(large_review.gaps, vec![CoverageGap::Oversized]);
    assert_eq!(
        large, small,
        "the rejection of a section must not first walk and index every item \
         in it: {SMALL} calls cost {small} bytes, {LARGE} cost {large}"
    );
}

/// **The positive control for the instrument, over the same sessions.**
///
/// When the bound has room, the section renders, and a rendered section is as
/// large as its items. If this cost does not grow, the counter does not count,
/// and the flat result above means nothing.
#[tokio::test]
async fn a_section_that_renders_does_cost_its_items() {
    let (small, small_review) = section_cost(SMALL, usize::MAX).await;
    let (large, large_review) = section_cost(LARGE, usize::MAX).await;
    eprintln!(
        "control, section rendered: {SMALL} calls -> {small} bytes, \
         {LARGE} calls -> {large} bytes"
    );
    assert!(small_review.gaps.is_empty(), "{:?}", small_review.gaps);
    assert!(large_review.gaps.is_empty(), "{:?}", large_review.gaps);
    // Each call and its result render as well over thirty-two bytes.
    let floor = ((LARGE - SMALL) * 32) as i64;
    assert!(
        large - small > floor,
        "the instrument must see item-shaped allocation when there is some: \
         {SMALL} calls -> {small} bytes, {LARGE} calls -> {large} bytes"
    );
}
