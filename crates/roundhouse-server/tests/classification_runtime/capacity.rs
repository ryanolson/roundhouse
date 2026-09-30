// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What one classification costs the admission path: how many store appends
//! a turn's own drain spends before its `TurnStarted` commit (server-2's
//! batching claim), and when the runtime's admission permit is taken and
//! released around a turn -- before dispatch, and back whatever the turn's
//! own outcome.

use super::settlement_repair::SettleOnceFailingLedger;
use super::*;

/// **server-2 control: a turn that drains nothing pays no store round trip
/// for the drain at all.** The batched commit is skipped entirely rather than
/// called with an empty batch, so the count of appends before `TurnStarted`
/// is zero whether or not this deployment ever classifies anything.
#[tokio::test]
async fn a_turn_with_nothing_parked_pays_no_store_append_before_routing() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&config(&base_url, true));
    let store = Arc::new(InstrumentedStore::new());
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let session = SessionId::new("sess_nothing_parked");

    // A warm-up turn over an engine with **no classifier attached**, so the
    // session exists (no `SessionCreated` commit ahead of the measured turn's
    // own `TurnStarted`) and, unlike a dispatched turn under the classifier
    // below, requests no classification of its own to race against the
    // measurement -- every dispatched turn requests one, so a warm-up under
    // the same classifier would leave a result parked exactly when this test
    // needs there to be none.
    let warm_up = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
        catalog(),
        Arc::new(registry),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    );
    warm_up.create_session(&session).await.unwrap();
    warm_up
        .run_turn(
            &session,
            TurnId::new("t0"),
            vec![Item::user_text("warm up")],
            &Admission::open(),
        )
        .await
        .expect("this fleet always answers");
    store.reset_calls();

    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await
        .expect("this fleet always answers");

    assert_eq!(
        store.calls_before_turn_started(),
        0,
        "nothing was parked, so the drain must not touch the store at all"
    );
}

/// **server-2: several parked results cost one store round trip, not one
/// each.** `deliver_classifier_output` batches every drained result into one
/// `record_background_classification` call rather than `k` sequential
/// appends for `k` parked results, every one of them before this turn's own
/// `TurnStarted` commit, on the path to first token. Three classifications
/// are held in flight together
/// (their ledger call gated open) so none can complete -- and be drained one
/// at a time by the turns that requested them -- before all three exist
/// simultaneously; only then are they released to park, and only then does a
/// fresh turn drain them.
#[tokio::test]
async fn a_turn_with_several_parked_results_pays_one_store_append_before_routing() {
    let (base_url, _upstream) = classifier_upstream().await;
    let mut classify = config(&base_url, true);
    classify.executor.max_in_flight = 3;
    // Three workers must reach `open_grant` independently of one another; the
    // default concurrency of 2 would strand the third behind the HTTP
    // semaphore, which nothing below ever releases before the ledger gate
    // does -- a deadlock this test would otherwise sit in until its own
    // timeout.
    classify.executor.max_http_concurrency = 3;
    let ledger = StallingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let store = Arc::new(InstrumentedStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_batched_drain");
    engine.create_session(&session).await.unwrap();

    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
        }
    };

    // Each turn requests its own classification; none can finish yet, since
    // every worker parks in the ledger's `open_grant` before it ever reaches
    // the classifier upstream.
    for id in ["t1", "t2", "t3"] {
        turn(id, "fix the parser")
            .await
            .expect("this fleet always answers");
    }
    for _ in 0..300 {
        if ledger.open_grant_calls.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ledger.open_grant_calls.load(Ordering::SeqCst),
        3,
        "all three calls are parked in the ledger together, so none has \
         completed and drained on its own turn"
    );
    ledger.release_open_grant();
    ledger.release_settle();

    for _ in 0..300 {
        if runtime.ready(&session).await.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.ready(&session).await.len(),
        3,
        "three results parked, undrained"
    );

    // Reset here, not only at the start: `t1`'s own `SessionCreated` and
    // `TurnStarted` commits, plus `t2` and `t3`'s own `TurnStarted` commits,
    // would otherwise be the first `true` this scan finds -- this test is
    // about `t4`'s drain, not about them.
    store.reset_calls();
    turn("t4", "add a test")
        .await
        .expect("this fleet always answers");

    assert_eq!(
        store.calls_before_turn_started(),
        1,
        "the three parked results are drained in one batched commit, not \
         three separate ones"
    );
}

/// **server-2: a parked result and a parked repair drain in one commit
/// together, not one each.** The test above only ever has results waiting; a
/// regression that split `deliver_classifier_output`'s batch back into "one
/// append for results, one for repairs" would still pass it, because a run
/// with no repair in flight cannot see the second append.
///
/// A session that owes a repair withholds its own new ticket
/// (`classification_before_turn`), so the two kinds cannot both arrive from
/// one session's *newest* turn the way an earlier version of this test built
/// them -- discovering the debt and buying a second call are mutually
/// exclusive within a single drain. The mixed state is still reachable, just
/// not from the newest turn: a call already dispatched before any debt
/// existed can still be sitting undelivered when an older debt's repair
/// finishes. `SettleOnceFailingLedger`'s ordinal gate on `open_grant` is what
/// lines that up deterministically -- t1 and t2 both dispatch and both park
/// in the ledger before either reaches the classifier, so t2's call exists
/// independently of whatever t1's turn later discovers.
#[tokio::test]
async fn a_turn_that_drains_a_result_and_a_repair_together_pays_one_store_append_before_routing() {
    let (base_url, _upstream) = classifier_upstream().await;
    let mut classify = config(&base_url, true);
    // Two calls held open at once below, each already holding its own HTTP
    // permit before it ever reaches the `open_grant` gate -- one spare over
    // that so a repair sharing the same executor is never the reason
    // something waits.
    classify.executor.max_http_concurrency = 3;
    let ledger = SettleOnceFailingLedger::new();
    let runtime = compose(
        "<test>",
        &classify,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let store = Arc::new(InstrumentedStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_result_and_repair_batched");
    engine.create_session(&session).await.unwrap();

    let turn = |id: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(id),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
        }
    };

    ledger.arm_open_grant_gate();

    // t1 and t2 each dispatch before either has any reason to owe anything --
    // t2 runs while t1's call is still parked in the ledger, so its own
    // drain sees nothing yet and takes a ticket of its own. Both calls now
    // park in `open_grant`, at ordinals 1 and 2, neither having reached the
    // classifier upstream at all.
    turn("t1", "fix the parser")
        .await
        .expect("this fleet always answers");
    for _ in 0..300 {
        if ledger.open_grant_calls() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    turn("t2", "add a test")
        .await
        .expect("this fleet always answers");
    for _ in 0..300 {
        if ledger.open_grant_calls() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Release only t1's call. It reaches the classifier, settles unconfirmed
    // -- the ledger fails its first attempt for every call id -- and parks
    // as a ready result. t2's call is still held, so it cannot yet be what
    // this produces.
    ledger.release_open_grant_through(1);
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.ready(&session).await.len(),
        1,
        "t1's classification completed and parked"
    );

    // t3 drains t1's result into the log -- which is the same turn that
    // discovers the session now owes a repair, withholds its own ticket, and
    // starts the repair in its `after_turn`. t2's call is still parked in
    // the ledger throughout, so nothing here is t3's own purchase.
    turn("t3", "one more")
        .await
        .expect("this fleet always answers");
    for _ in 0..300 {
        if !runtime.ready_repairs(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.ready_repairs(&session).await.len(),
        1,
        "t1's repair settled and parked"
    );

    // Now release t2's call, dispatched back before t1's turn ever owed
    // anything. It reaches the classifier and settles unconfirmed in its own
    // right -- a second debt, not yet delivered -- and parks as a second
    // ready result, independent of the repair above.
    ledger.release_open_grant_through(2);
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty()
            && !runtime.ready_repairs(&session).await.is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.ready(&session).await.len(),
        1,
        "t2's own classification parked, undrained"
    );
    assert_eq!(
        runtime.ready_repairs(&session).await.len(),
        1,
        "t1's repair is still parked, undrained"
    );

    // t4 drains both together.
    store.reset_calls();
    turn("t4", "and another")
        .await
        .expect("this fleet always answers");

    assert_eq!(
        store.calls_before_turn_started(),
        1,
        "a parked result and a parked repair are drained in one batched \
         commit together, not one append each"
    );
}

/// **The permit is taken before the payload, and held for it.**
///
/// A classification's admission is decided while the turn still holds its own
/// items -- which is the only moment the bounded prompt copy can be taken -- so
/// by the time the turn is on the wire the permit that will carry its
/// classification is already spent. A permit taken after the answer came back
/// would leave this turn's capture built on a queue that may have no room for
/// it, and the whole capture paid for nothing.
#[tokio::test]
async fn the_capacity_for_a_classification_is_taken_before_the_turn_is_dispatched() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&config(&base_url, true));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Watchful {
            runtime: Arc::clone(&runtime),
            observed: Arc::clone(&observed),
        }) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );

    let free = runtime.limits().max_in_flight;
    assert_eq!(runtime.available_capacity(), free, "nothing held yet");

    let session = SessionId::new("sess_permit_first");
    engine.create_session(&session).await.unwrap();
    engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await
        .expect("this fleet always answers");

    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[free - 1],
        "the classification's permit must already be held while the turn it \
         describes is on the wire, because the payload it admits was taken \
         before the turn was even committed"
    );
}

/// **A deduplicated turn gives back what it took.**
///
/// The client's retry of a completed turn returns before anything is classified
/// -- and the permit it took on the way in has to come back, or a retry would
/// cost a deployment a classification slot per retry. Asserted on the next
/// turn's own classification rather than only on the gauge: with one slot
/// configured, a leaked permit is a classifier that never runs again.
#[tokio::test]
async fn a_deduplicated_turn_releases_the_capacity_it_took() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&one_slot(&base_url));
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_dedup_capacity");
    engine.create_session(&session).await.unwrap();
    let turn = |name: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine
                .run_turn(
                    &session,
                    TurnId::new(name),
                    vec![Item::user_text("fix the parser")],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers")
        }
    };

    let first = turn("t1").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.available_capacity(),
        0,
        "the one slot is occupied by the undelivered result"
    );

    // The same turn id again: the client's retry, replayed from the log. Its
    // drain frees the first call's slot, and whatever the retry itself took has
    // to come back too.
    let replayed = turn("t1").await;
    assert!(
        replayed.deduplicated,
        "the fixture must actually deduplicate"
    );
    assert_eq!(replayed.response_id, first.response_id);
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        1,
        "a retry buys no second answer"
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and the retry released the slot it took on the way in"
    );

    turn("t2").await;
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        2,
        "so the next real turn can still be classified"
    );
}

/// **A turn that failed gives back what it took.**
///
/// Nothing is classified without a decision to classify under, so a dispatch
/// that never completed writes no intent -- and the slot it took while it was
/// trying has to be free for the turn that follows it.
#[tokio::test]
async fn a_failed_turn_releases_the_capacity_it_took() {
    let (base_url, _upstream) = classifier_upstream().await;
    let runtime = runtime_with(&one_slot(&base_url));
    let store = Arc::new(MemoryStore::new());
    let failing = Arc::new(FailsOnce::new());
    let engine = engine_over(
        Arc::clone(&store),
        Arc::clone(&failing) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_failed_capacity");
    engine.create_session(&session).await.unwrap();

    let failed = engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await;
    assert!(failed.is_err(), "the fixture must actually fail the turn");
    assert!(
        intents_in(store.as_ref(), &session).await.is_empty(),
        "a turn that never completed is not classified"
    );
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and the slot it took before the dispatch is back"
    );

    engine
        .run_turn(
            &session,
            TurnId::new("t2"),
            vec![Item::user_text("try again")],
            &Admission::open(),
        )
        .await
        .expect("the second attempt answers");
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        1,
        "so the next turn can still be classified"
    );
}

/// **An intent the log refused gives back what it took.**
///
/// The durable intent is written before anything may be sent, so an append that
/// fails means no call -- and the slot reserved for that call must not be lost
/// with it. A store outage that cost a deployment its classification capacity
/// permanently would be an outage that outlived itself.
#[tokio::test]
async fn an_intent_the_log_refused_releases_the_capacity_it_took() {
    let (base_url, upstream) = classifier_upstream().await;
    let runtime = runtime_with(&one_slot(&base_url));
    let store = RefusingStore::new();
    let engine = engine_over(
        Arc::clone(&store),
        Arc::new(Answering) as Arc<dyn FrontierClient>,
        Arc::clone(&runtime),
    );
    let session = SessionId::new("sess_refused_intent");
    engine.create_session(&session).await.unwrap();

    store.refuse_intents(true);
    engine
        .run_turn(
            &session,
            TurnId::new("t1"),
            vec![Item::user_text("fix the parser")],
            &Admission::open(),
        )
        .await
        .expect("the refusal is the classifier's, and does not fail the turn");
    assert!(
        intents_in(store.as_ref(), &session).await.is_empty(),
        "the append really did fail"
    );
    assert_eq!(upstream.count(), 0, "so nothing was sent");
    assert_eq!(
        runtime.available_capacity(),
        1,
        "and the slot that call would have used is back"
    );

    store.refuse_intents(false);
    engine
        .run_turn(
            &session,
            TurnId::new("t2"),
            vec![Item::user_text("now add a test")],
            &Admission::open(),
        )
        .await
        .expect("this fleet always answers");
    assert_eq!(
        intents_in(store.as_ref(), &session).await.len(),
        1,
        "so the next turn can still be classified"
    );
}

/// Fails the first dispatch the way a provider that is not there fails, and
/// answers every one after it.
struct FailsOnce {
    calls: AtomicUsize,
}

impl FailsOnce {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl FrontierClient for FailsOnce {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(FrontierError::Transport {
                message: "connection refused".into(),
                timed_out: false,
            }),
            _ => Ok(FrontierChunk::whole_response(
                "done".to_string(),
                quote.prompt.len() as u64,
                0,
                CacheReadSource::Provider,
                4,
                0,
            )),
        }
    }
}
