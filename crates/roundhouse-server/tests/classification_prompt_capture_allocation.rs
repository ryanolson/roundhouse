// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract: a classifier that cannot take the work does not make the turn pay
//! for a payload it will never submit.
//!
//! `Engine::run_turn` holds the turn's items once, and the bounded copy of the
//! prompt has to be taken there or not at all. What decides whether it is taken
//! is admission: a saturated queue, a stopped runtime and a deployment with no
//! classifier at all are three ways of saying "there is no room for this call",
//! and none of them may cost the serving turn a copy of the user's prompt.
//! The permit is what makes that decidable before the copy exists — taken
//! before the capture, held across the turn, and covering the captured payload
//! through queueing, execution and delivery.
//!
//! `PromptCapture::of` has no side effect a black-box test can key on -- no
//! HTTP, no ledger touch, nothing durable if the call it would have fed never
//! happens -- so the only way to *observe* whether it ran is to measure what it
//! does: allocate a bounded copy of the current turn's prompt, whose size is
//! the configured `max_prompt_chars`. A custom global allocator, scoped to this
//! one integration-test binary, makes that observable without touching
//! production code.
//!
//! **Each "did not build it" claim is paired with a control that builds it.**
//! Two saturated turns at different caps allocating the same amount is exactly
//! what a broken instrument reports, so the same two caps are also run through
//! a classifier that *does* have room, where the allocation must scale with the
//! cap. One without the other proves nothing.
//!
//! The counter is **per thread, not process-wide**. `cargo test` gives every
//! test its own OS thread by default, and a plain `#[tokio::test]` (no
//! `flavor` given, so `current_thread`) never migrates that test's work --
//! including anything it spawns -- off that one thread. A thread-local
//! counter therefore sees exactly this test's allocations and nothing any
//! concurrently-running sibling test does; a process-wide counter gated by a
//! mutex around the *measured region* is not enough, because a sibling's
//! ordinary, unmeasured allocations on its own thread would still land in
//! the same global total for as long as any measurement anywhere is open.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::MemorySpendLedger;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{EchoFrontierClient, FrontierClient, WireProtocol};
use roundhouse_server::classify_config::ClassifyConfig;
use roundhouse_server::classify_runtime::{ClassificationRuntime, compose};
use roundhouse_server::test_support::{engine_over_echo, frontier_spec, single_model_catalog};
use roundhouse_server::{Admission, EngineConfig};

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
/// this thread alone -- safe across every `.await` in `body` because a
/// `current_thread` tokio runtime never moves this test's work to another
/// thread.
async fn measure<F, Fut, T>(body: F) -> (T, usize)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let before = LOCAL_ALLOCATED.with(|allocated| allocated.get());
    LOCAL_COUNTING.with(|counting| counting.set(counting.get() + 1));
    let result = body().await;
    LOCAL_COUNTING.with(|counting| counting.set(counting.get() - 1));
    let after = LOCAL_ALLOCATED.with(|allocated| allocated.get());
    (result, after.saturating_sub(before))
}

const ANSWER: &str = "frontier answer";
const PROVIDER: &str = "capture-alloc";
/// Comfortably past even the larger cap used below, so a capture that *is*
/// taken is known to copy the *full* configured cap rather than stopping short
/// because the prompt itself was smaller.
const PROMPT_LEN: usize = 80_000;
const SMALL_CAP: usize = 2_000;
const LARGE_CAP: usize = 50_000;
/// The same session id under every run, each over its own fresh store, so two
/// measurements differ by the thing under test and not by the length of a name.
const SESSION: &str = "sess_capture_allocation";

/// What a turn is allowed to spend on a classifier that cannot take it.
///
/// Well under a capture of either cap and well over the handful of bytes an
/// empty classification window and a `ready` probe cost, so it separates "no
/// capture was built" from "a capture was built" without pinning either run to
/// an exact byte count. Every assertion below also prints its raw numbers, so
/// the margin is readable rather than trusted.
const INCIDENTAL: usize = SMALL_CAP / 2;

fn env(name: &str) -> Option<String> {
    match name {
        "CLASSIFY_CAPTURE_ALLOC_TEST_KEY" => Some("sk-capture-alloc-test".to_string()),
        _ => None,
    }
}

/// A classifier config with room for exactly one classification in flight
/// and a caller-chosen `max_prompt_chars`. Where a test wants saturation it
/// holds that one permit itself before its measured turn runs, so
/// `ClassificationRuntime::capacity()` returns `None` for the turn under
/// measurement -- `max_in_flight: 0` is not an option here;
/// `ClassifyConfig::from_json` rejects a zero on any executor axis as invalid
/// configuration, which is itself the right call for a deployment file and not
/// something worth working around in production code just to make this test
/// simpler.
fn one_slot_classify_config(
    base_url: &str,
    max_prompt_chars: usize,
    enabled: bool,
) -> ClassifyConfig {
    roundhouse_server::test_support::classification::classify_config(base_url, |value| {
        value["enabled"] = serde_json::json!(enabled);
        value["revision"] = serde_json::json!(1);
        value["auth"]["env"] = serde_json::json!("CLASSIFY_CAPTURE_ALLOC_TEST_KEY");
        value["caps"]["max_prompt_chars"] = serde_json::json!(max_prompt_chars);
        value["caps"]["max_total_bytes"] = serde_json::json!(400_000);
        value["transport"]["max_request_bytes"] = serde_json::json!(400_000);
        value["executor"]["max_in_flight"] = serde_json::json!(1);
        value["executor"]["max_http_concurrency"] = serde_json::json!(1);
        value["executor"]["sweep_interval_ms"] = serde_json::json!(50000);
    })
}

fn catalog() -> roundhouse_fleet::StaticFrontierCatalog {
    single_model_catalog(frontier_spec(PROVIDER, "m", WireProtocol::OpenAiResponses))
}

/// A composed runtime at `max_prompt_chars`.
///
/// The base URL is one nothing needs to be reachable at: every measured turn
/// below either has no permit to spend or is the control, whose one call is
/// refused at the socket and whose payload has already been built by then.
fn runtime(max_prompt_chars: usize) -> Arc<ClassificationRuntime<ByteTokenizer>> {
    compose(
        "<test>",
        &one_slot_classify_config("http://127.0.0.1:1", max_prompt_chars, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present")
}

/// One turn of an 80,000-character prompt, measured end to end, over a fresh
/// store and a fleet that always answers.
async fn run_one_turn(classifier: Option<Arc<ClassificationRuntime<ByteTokenizer>>>) -> usize {
    let store = Arc::new(MemoryStore::new());
    let mut engine = engine_over_echo(
        Arc::clone(&store),
        catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)) as Arc<dyn FrontierClient>,
        EngineConfig::default(),
    );
    if let Some(classifier) = classifier {
        engine = engine.with_classifier(classifier);
    }
    let session = SessionId::new(SESSION);
    engine.create_session(&session).await.unwrap();
    let prompt = "x".repeat(PROMPT_LEN);
    let (_, bytes) = measure(|| async {
        engine
            .run_turn(
                &session,
                TurnId::new("t1"),
                vec![Item::user_text(prompt)],
                &Admission::open(),
            )
            .await
            .expect("the echo fleet always answers")
    })
    .await;
    bytes
}

/// **The required behaviour: a saturated classifier's turn does not pay for a
/// capture.**
///
/// Two turns that are both saturated-classifier turns, differing only in the
/// configured `max_prompt_chars`. Every allocation that comes from having a
/// classifier configured at all -- the runtime, its semaphores, the HTTP
/// client, the parsed config -- is built outside the measured region and is
/// identical between the two runs, so a delta that tracked `max_prompt_chars`
/// could only be the bounded copy of the prompt. There is no room for the call,
/// so there must be no copy, so there must be no delta.
#[tokio::test]
async fn a_saturated_classifier_builds_no_prompt_capture() {
    let small_runtime = runtime(SMALL_CAP);
    let _small_held = small_runtime.capacity().expect("the one configured permit");
    assert!(
        small_runtime.capacity().is_none(),
        "the classifier must already be saturated before the turn runs"
    );
    let small = run_one_turn(Some(Arc::clone(&small_runtime))).await;

    let large_runtime = runtime(LARGE_CAP);
    let _large_held = large_runtime.capacity().expect("the one configured permit");
    let large = run_one_turn(Some(Arc::clone(&large_runtime))).await;

    eprintln!(
        "saturated capture allocation: cap={SMALL_CAP} -> {small} bytes, \
         cap={LARGE_CAP} -> {large} bytes (delta {})",
        large.saturating_sub(small)
    );

    assert!(
        large <= small + INCIDENTAL,
        "a saturated classifier's turn must not allocate with a cap it will \
         never use: cap={SMALL_CAP} allocated {small} bytes, cap={LARGE_CAP} \
         allocated {large} bytes -- a difference of at most {INCIDENTAL} is \
         what having no capture at either cap looks like"
    );
}

/// **The positive control, and the other half of the contract.**
///
/// The same two caps through a classifier that *does* have room. A permit is
/// there to be taken, so the capture is built and submitted, and its
/// allocation therefore scales with the cap -- which is what proves the
/// instrument above is live and that the fix skipped the capture rather than
/// the measurement.
#[tokio::test]
async fn an_admitting_classifier_does_build_the_capture_it_submits() {
    let small = run_one_turn(Some(runtime(SMALL_CAP))).await;
    let large = run_one_turn(Some(runtime(LARGE_CAP))).await;

    eprintln!(
        "control, admitting capture allocation: cap={SMALL_CAP} -> {small} bytes, \
         cap={LARGE_CAP} -> {large} bytes (delta {})",
        large.saturating_sub(small)
    );

    let expected_delta = LARGE_CAP - SMALL_CAP;
    assert!(
        large > small + expected_delta / 2,
        "a classifier with room must still take the bounded copy it submits: \
         cap={SMALL_CAP} allocated {small} bytes, cap={LARGE_CAP} allocated \
         {large} bytes -- expected at least {} more",
        expected_delta / 2
    );
}

/// **A saturated classifier costs the turn no more than no classifier at
/// all.**
///
/// Coarser than the cap comparison (it also carries whatever a configured
/// classifier costs a turn beyond the capture: the `ready` probe and the empty
/// classification window), and from a different angle -- the shipped state,
/// which has no classifier, is the baseline a saturated one must not exceed by
/// a prompt copy.
#[tokio::test]
async fn a_saturated_classifier_costs_the_turn_no_more_than_no_classifier_at_all() {
    let without = run_one_turn(None).await;

    let saturated = runtime(LARGE_CAP);
    let _held = saturated.capacity().expect("the one configured permit");
    let with_saturated = run_one_turn(Some(Arc::clone(&saturated))).await;

    eprintln!(
        "capture allocation: classifier absent -> {without} bytes, \
         classifier saturated -> {with_saturated} bytes (delta {})",
        with_saturated.saturating_sub(without)
    );

    assert!(
        with_saturated <= without + INCIDENTAL,
        "a saturated classifier must cost this turn no prompt copy: absent \
         allocated {without} bytes, saturated allocated {with_saturated} \
         bytes -- a difference of at most {INCIDENTAL} is what no capture \
         looks like"
    );
}

/// **A stopped runtime is the third way there is no room**, and it must read
/// the same as the other two.
///
/// A lifetime that has ended refuses admission, so a turn arriving after it
/// builds nothing -- the same statement the saturated case makes, through the
/// other branch of `capacity()`.
#[tokio::test]
async fn a_stopped_classifier_builds_no_prompt_capture() {
    let without = run_one_turn(None).await;

    let stopped = runtime(LARGE_CAP);
    stopped.stop();
    assert!(
        stopped.capacity().is_none(),
        "a stopped runtime admits nothing"
    );
    let with_stopped = run_one_turn(Some(Arc::clone(&stopped))).await;

    eprintln!(
        "capture allocation: classifier absent -> {without} bytes, \
         classifier stopped -> {with_stopped} bytes (delta {})",
        with_stopped.saturating_sub(without)
    );

    assert!(
        with_stopped <= without + INCIDENTAL,
        "a stopped classifier must cost this turn no prompt copy: absent \
         allocated {without} bytes, stopped allocated {with_stopped} bytes"
    );
}

/// **A disabled configuration composes no runtime at all**, which is why the
/// engine that serves it is the no-classifier engine measured above rather
/// than a fourth case needing its own bound.
#[tokio::test]
async fn a_disabled_configuration_composes_no_runtime_to_capture_with() {
    let composed = compose(
        "<test>",
        &one_slot_classify_config("http://127.0.0.1:1", LARGE_CAP, false),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("a disabled file is still a valid file");
    assert!(
        composed.is_none(),
        "a disabled classifier reaches the engine as no classifier, so there is \
         nothing to take a capture with"
    );
}

/// **A saturated classifier writes no durable intent and makes no HTTP
/// call.**
///
/// The durability half of the same admission decision: with no permit there is
/// no payload, and with no payload there is nothing to record an intent for or
/// send. Kept live rather than left to the reading of `run_turn`, because an
/// allocation measurement cannot show that the log and the socket stayed quiet.
#[tokio::test]
async fn a_saturated_classifier_writes_no_intent_and_makes_no_http_call() {
    let (base_url, upstream_calls) = {
        use axum::Router;
        use axum::body::Body;
        use axum::response::Response;
        use axum::routing::post;
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let app = Router::new().route(
            "/systemone",
            post(move || {
                let counted = Arc::clone(&counted);
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Response::new(Body::from("{}"))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), calls)
    };

    let runtime = compose(
        "<test>",
        &one_slot_classify_config(&base_url, LARGE_CAP, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let _held = runtime.capacity().expect("the one configured permit");

    let store = Arc::new(MemoryStore::new());
    let engine = engine_over_echo(
        Arc::clone(&store),
        catalog(),
        Arc::new(EchoFrontierClient::new(ANSWER)) as Arc<dyn FrontierClient>,
        EngineConfig::default(),
    )
    .with_classifier(runtime);
    let session = SessionId::new("sess_no_intent_no_http");
    engine.create_session(&session).await.unwrap();
    engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await
        .expect("the echo fleet always answers");

    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    assert!(
        !events.iter().any(|event| matches!(
            event.kind,
            roundhouse_core::event::SessionEventKind::ClassificationRequested { .. }
        )),
        "no permit, so no payload, so no durable intent"
    );
    assert_eq!(
        upstream_calls.load(Ordering::SeqCst),
        0,
        "and in particular no HTTP request was ever attempted"
    );
}
