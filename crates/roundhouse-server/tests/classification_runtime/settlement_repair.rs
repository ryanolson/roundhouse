// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What survives a failed write: a classification result whose own append is
//! refused stays with the runtime for a later turn to deliver, and an
//! unconfirmed settlement is repaired -- once, without a second purchase --
//! by whichever later turn's writer succeeds.

use super::*;

/// **A failed append keeps the answer and buys nothing further.**
///
/// The result stays with the runtime — still holding its admission permit, so
/// the bound still counts it — and the next turn whose writer works delivers it.
/// No second provider call: the answer is already bought.
#[tokio::test]
async fn a_failed_append_retains_the_result_for_a_later_turn() {
    let (base_url, upstream) = classifier_upstream().await;
    let store = RefusingStore::new();
    let runtime = compose(
        "<test>",
        &config(&base_url, true),
        Arc::new(MemorySpendLedger::new()),
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let engine = Arc::new(
        Engine::with_provider_clients(
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
        )
        .with_classifier(Arc::clone(&runtime)),
    );
    let session = SessionId::new("sess_append_fails");
    let turn = |name: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            engine.create_session(&session).await.unwrap();
            engine
                .run_turn(
                    &session,
                    TurnId::new(name),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                )
                .await
                .expect("this fleet always answers");
        }
    };

    turn("t1", "fix the parser").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let calls_after_first = upstream.count();
    assert_eq!(calls_after_first, 1);
    let held = runtime.available_capacity();

    // The second turn's writer refuses the result.
    store.refuse_results(true);
    turn("t2", "add a test").await;
    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ClassificationRecorded { .. })),
        "the append really did fail"
    );
    assert!(
        !runtime.ready(&session).await.is_empty(),
        "and the result stayed with the runtime rather than being acknowledged"
    );
    assert!(
        runtime.available_capacity() <= held,
        "a retained result still occupies its admission permit"
    );

    // The third turn's writer works, and delivers it.
    store.refuse_results(false);
    turn("t3", "and another").await;
    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    let delivered: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ClassificationRecorded { record } => Some(record.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !delivered.is_empty(),
        "a later turn's writer delivers what the failed one could not"
    );
    assert_eq!(
        delivered[0]
            .outcome
            .classification()
            .expect("the answer survived the failed append")
            .intent
            .value,
        TurnIntent::Implement
    );
    // **No second answer was bought for turn one's question.** Asserted on the
    // durable intents rather than on the classifier's call count, which the
    // later turns' own background workers are still racing: one intent per
    // turn, three turns, and the first turn's call named exactly once.
    let intents: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ClassificationRequested { record } => Some(record.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(intents.len(), 3, "one intent per turn and no more");
    let mut sources: Vec<_> = intents
        .iter()
        .map(|intent| intent.source_response_id.clone())
        .collect();
    sources.sort_by_key(|id| id.to_string());
    sources.dedup();
    assert_eq!(
        sources.len(),
        3,
        "a re-asked question would show up as two intents naming one turn"
    );
    assert_eq!(
        intents
            .iter()
            .filter(|intent| intent.call_id == delivered[0].call_id)
            .count(),
        1,
        "and the delivered answer's call was issued exactly once"
    );
    assert!(
        upstream.count() <= 3,
        "at most one provider call per turn, whatever the delivery did"
    );
}

// ------------------------------------------------------ settlement repair

/// An evaluation ledger whose `settle_grant` fails the *first* attempt for a
/// given `response_id` and delegates normally to a real [`MemorySpendLedger`]
/// on every attempt after that.
///
/// So the original call's settle always fails and a repair's always succeeds,
/// which is what makes the test below about *whether anything retries* rather
/// than about the double.
///
/// `pub(crate)`: `capacity.rs`'s batching test reuses it too, to park a
/// result and a repair for the same session at once -- the one shape that
/// needs a call whose settle fails on arrival and succeeds on retry.
pub(crate) struct SettleOnceFailingLedger {
    inner: MemorySpendLedger,
    settle_calls: AtomicUsize,
    failed_once: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Every `(call, amount)` this ledger actually applied.
    ///
    /// Per call rather than in total, because this rig has no local fleet: every
    /// turn routes to the frontier and so buys its own classification, and a
    /// project-wide committed figure here would be the sum of an unknown number
    /// of unrelated calls. The subject is one call's recovery, so the evidence
    /// is keyed by that call.
    applied: std::sync::Mutex<Vec<(String, f64)>>,
}

impl SettleOnceFailingLedger {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            settle_calls: AtomicUsize::new(0),
            failed_once: std::sync::Mutex::new(std::collections::HashSet::new()),
            applied: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn settle_calls(&self) -> usize {
        self.settle_calls.load(Ordering::SeqCst)
    }

    /// What this ledger committed for one call, and `None` if it never did.
    fn applied_usd(&self, call_id: &str) -> Option<f64> {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == call_id)
            .map(|(_, usd)| *usd)
    }

    fn applications(&self, call_id: &str) -> usize {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == call_id)
            .count()
    }
}

#[async_trait]
impl roundhouse_core::control::SpendLedger for SettleOnceFailingLedger {
    async fn open_grant(
        &self,
        request: roundhouse_core::control::GrantRequest,
    ) -> Result<roundhouse_core::control::Grant, roundhouse_core::control::SpendError> {
        self.inner.open_grant(request).await
    }

    async fn settle_grant(
        &self,
        settlement: roundhouse_core::control::Settlement,
    ) -> Result<roundhouse_core::control::Settled, roundhouse_core::control::SpendError> {
        self.settle_calls.fetch_add(1, Ordering::SeqCst);
        let first_attempt = self
            .failed_once
            .lock()
            .unwrap()
            .insert(settlement.response_id.to_string());
        if first_attempt {
            return Err(roundhouse_core::control::SpendError::Backend(
                anyhow::anyhow!("simulated transient settlement failure"),
            ));
        }
        let call_id = settlement.response_id.to_string();
        let actual_usd = settlement.actual_usd;
        let settled = self.inner.settle_grant(settlement).await?;
        if settled.applied {
            self.applied.lock().unwrap().push((call_id, actual_usd));
        }
        Ok(settled)
    }

    async fn balance(
        &self,
        query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, roundhouse_core::control::SpendError> {
        self.inner.balance(query).await
    }
}

/// **A settlement nobody acknowledged is recovered by a later turn, under its
/// original identity and its original measured amount, with no second
/// purchase.**
///
/// The classifier call itself succeeds (usage is reported, `spend` is
/// `Measured`); only the settle fails, which is the one condition
/// `SettlementAck::Unconfirmed` exists to record. `engine/spend.rs` names a
/// `repair_settle` that runs on every session open for the *serving* ledger;
/// this is the evaluation ledger's counterpart, and until it existed the
/// question stayed open forever — the record said the charge was unconfirmed
/// and nothing ever asked again, so whether the ledger had applied it was
/// unknowable rather than known to be lost.
///
/// The durable record keeps reading `Unconfirmed`, and that is deliberate
/// rather than an omission: it is what the *call* established, and it is still
/// true of the call. What the repair adds is a separate event saying the
/// question was later resolved. Rewriting the first record would destroy the
/// evidence that a settle had failed at all.
///
/// `classification_settlement_recovery.rs` is where the full matrix lives —
/// restart, reprice, monthly reset, missing usage, payer attribution. This one
/// keeps the claim in the suite that used to assert its opposite.
#[tokio::test]
async fn an_unconfirmed_settlement_is_repaired_by_a_later_turn_without_a_second_purchase() {
    let (base_url, upstream) = classifier_upstream().await;
    let classify = config(&base_url, true);
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
    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::with_provider_clients(
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
        )
        .with_classifier(Arc::clone(&runtime)),
    );
    let session = SessionId::new("sess_settle_repair");
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
                .expect("this fleet always answers");
        }
    };

    turn("t1", "fix the parser").await;
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parked = runtime.ready(&session).await;
    assert_eq!(parked.len(), 1, "the classification completed and parked");
    let spend = parked[0]
        .record
        .outcome
        .spend()
        .expect("a call was actually attempted");
    assert_eq!(
        spend.settled(),
        roundhouse_core::classify::SettlementAck::Unconfirmed,
        "the first settlement genuinely went unacknowledged"
    );
    let measured_usd = match spend {
        roundhouse_core::classify::EvaluationSpend::Measured { usd, .. } => *usd,
        other => panic!("the classifier answered, so usage must be measured: {other:?}"),
    };
    // The identity everything below is keyed by. This rig has no local fleet,
    // so later turns route to the frontier and buy classifications of their
    // own; only this call's settlement is the subject.
    let call_id = parked[0].record.call_id.to_string();
    drop(parked);
    assert_eq!(ledger.settle_calls(), 1);
    assert_eq!(
        ledger.applied_usd(&call_id),
        None,
        "and nothing is committed for it yet"
    );

    // `t2` drains the parked result into the log as `ClassificationRecorded`
    // and, at its tail, starts the repair. `t3` is what commits the repair's
    // acknowledgement. Bounded polling rather than a sleep, and on the
    // *committed total* rather than on a call count: the charge lands when the
    // background settle returns, which is not synchronous with either turn.
    turn("t2", "add a test").await;
    turn("t3", "one more").await;
    let mut recovered = None;
    for _ in 0..300 {
        recovered = ledger.applied_usd(&call_id);
        if recovered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        recovered,
        Some(measured_usd),
        "the original measured amount, recovered under the original call \
         identity -- not a re-derived price and not a later turn's"
    );
    assert_eq!(
        ledger.applications(&call_id),
        1,
        "and applied exactly once, however many turns replayed the log after it"
    );

    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    let recorded = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::ClassificationRecorded { record }
                if record.call_id.to_string() == call_id =>
            {
                Some(record.clone())
            }
            _ => None,
        })
        .expect("the result is durable");
    assert_eq!(
        recorded
            .outcome
            .spend()
            .expect("a spend was attempted")
            .settled(),
        roundhouse_core::classify::SettlementAck::Unconfirmed,
        "and it still reads what the call established: the repair is a \
         separate event, not a rewrite of the evidence"
    );

    // **A repair settles; it never sends.** Stated as a relation rather than as
    // a hard-coded count, because this rig's later turns legitimately classify:
    // every purchase must be accounted for by a durable intent, so a repair
    // that issued a request would show up here as a call nobody committed to.
    let intents = intents_in(&*store, &session).await;
    assert_eq!(
        upstream.count(),
        intents.len(),
        "every classifier request is accounted for by a durable intent, so \
         recovering a settlement bought nothing"
    );
    assert!(
        intents
            .iter()
            .any(|intent| intent.call_id.to_string() == call_id),
        "including the one whose settlement this test recovered"
    );
}

/// **server-3 red: a turn cancelled between a repair's durable append and its
/// acknowledgement must not re-append that repair on the next turn.**
///
/// `deliver_classifications` already guards results against exactly this
/// shape of crash: `classification_settled` is checked before any append
/// (`engine/classification.rs`, the `deliver_classifications` loop).
/// `deliver_settlement_repairs` applies no equivalent guard. A turn cancelled
/// after `record_classification_settlement_repair` commits but before
/// `acknowledge_repairs` runs leaves the runtime still holding the handle, so
/// the next turn's drain finds it "ready" again and appends a second
/// `ClassificationSettlementRepaired` for a settlement the log already
/// records as repaired.
#[tokio::test]
async fn a_turn_cancelled_after_a_repairs_append_does_not_re_append_it_next_turn() {
    let (base_url, _upstream) = classifier_upstream().await;
    let classify = config(&base_url, true);
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
    let session = SessionId::new("sess_repair_cancel");
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

    // t1's call settles unconfirmed: the ledger fails its first attempt.
    turn("t1", "fix the parser")
        .await
        .expect("this fleet always answers");
    for _ in 0..300 {
        if !runtime.ready(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.ready(&session).await.len(),
        1,
        "the classification completed and parked"
    );

    // t2 drains the result into the log and, at its tail, starts the repair —
    // which succeeds this time, since the ledger only fails once.
    turn("t2", "add a test")
        .await
        .expect("this fleet always answers");
    for _ in 0..300 {
        if !runtime.ready_repairs(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parked_repairs = runtime.ready_repairs(&session).await;
    assert_eq!(parked_repairs.len(), 1, "the repair settled and parked");
    let repair_call_id = parked_repairs[0].record.call_id.to_string();
    drop(parked_repairs);

    // t3 drains the repair — its append lands durably — and is cancelled
    // right there, before `acknowledge_repairs` can run.
    store.stall_next_repair_append();
    tokio::select! {
        _ = store.repair_appended.notified() => {}
        result = turn("t3", "one more") => {
            panic!("t3 must stall on its repair append, not complete: {result:?}");
        }
    }

    // The cancelled turn's `Session` was dropped holding the lease; nothing
    // released it, so the next turn must not find it still held.
    store.inner.expire_lease_now(&session).await;

    assert_eq!(
        repair_events_for(&*store, &session, &repair_call_id).await,
        1,
        "red-test control: the append that landed before cancellation is the \
         only one so far"
    );
    assert_eq!(
        runtime.retained_repairs(&session).await,
        1,
        "the runtime still holds the un-acknowledged handle -- acknowledge_repairs \
         never ran"
    );

    // t4 is an ordinary turn. Its drain sees the same repair still "ready" —
    // acknowledge_repairs never ran — and must not append it a second time
    // merely because the log already recorded it.
    turn("t4", "final")
        .await
        .expect("this fleet always answers");

    assert_eq!(
        repair_events_for(&*store, &session, &repair_call_id).await,
        1,
        "a settlement already repaired in the log must not be repaired twice \
         merely because a cancelled turn lost the acknowledgement that would \
         have released it"
    );
}

async fn repair_events_for(store: &impl SessionStore, session: &SessionId, call_id: &str) -> usize {
    store
        .read_events(session, 0, 1_000)
        .await
        .expect("a log reads")
        .into_iter()
        .filter(|event| {
            matches!(
                &event.kind,
                SessionEventKind::ClassificationSettlementRepaired { record }
                    if record.call_id.to_string() == call_id
            )
        })
        .count()
}
