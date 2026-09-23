// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Recovery claims across a restart and a replay: the before-apply and
//! after-apply failure modes, and a durable repair acknowledgement whose own
//! append fails. Split from classification_settlement_recovery.rs (server-7,
//! PR 18 round 1) to keep each claim's file under 1000 lines; the shared
//! fixtures -- the loopback classifier, the fleet, RiggedLedger, the rig, and
//! the log-reading helpers -- stay in the parent module, reached here through
//! use super::*.

use super::*;

// --------------------------------------------------------------------- claims

/// **A settle that failed before the ledger applied it recovers its original
/// charge, after a restart, from the log alone.**
///
/// The classifier answered and billed; the settle never reached the accounting.
/// A successor process replaying this log has everything it needs — the call's
/// identity, its payer, the price the record holds and the window the intent
/// recorded — and must put the charge where the first process could not.
#[tokio::test]
async fn a_settlement_that_failed_before_application_recovers_after_a_restart() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_before_apply");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "the premise: the failed settle left the ledger untouched"
    );

    // The restart. A fresh engine and a fresh classification runtime over the
    // same persisted log and the same durable ledger.
    let restarted = deployment(&store, &ledger, &config).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed, usd,
        "the original measured price, recovered from the durable record"
    );
    assert_eq!(
        upstream.count(),
        1,
        "and recovered without buying the answer again"
    );
}

/// **An acknowledgement lost after the ledger applied the settle is resolved,
/// and resolving it charges nothing further.**
///
/// The failure the record cannot tell from the one above: the backend applied
/// the settle and then failed on the way back. Recovery must re-drive it — the
/// process genuinely does not know — and the ledger's `applied: false` is the
/// answer that resolves it. Treating that as a failure and charging again is
/// the one outcome worse than never repairing at all.
///
/// The load-bearing assertion is that the settle is **re-driven at all**.
/// Committed dollars already equal the price before recovery, so a test that
/// only checked the total would pass against a codebase that does nothing.
#[tokio::test]
async fn a_lost_acknowledgement_after_application_is_resolved_without_charging_twice() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::AfterApply);
    let session = SessionId::new("sess_lost_ack");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "the premise: the charge did land, and this process never learned it"
    );
    let settles_before = ledger.settle_calls();

    let restarted = deployment(&store, &ledger, &config).await;
    let found = drive_until_repaired(&restarted, &store, &session, &["t3", "t4"]).await;

    assert_eq!(
        found.len(),
        1,
        "the unconfirmed settlement is acknowledged exactly once in the log"
    );
    assert!(
        ledger.settle_calls() > settles_before,
        "the ledger must actually be asked again -- the record alone cannot \
         distinguish a lost acknowledgement from a charge that never landed"
    );
    assert!(
        ledger.deduplicated(call_id.as_str()),
        "and the ledger's answer must be `applied: false`: it already had this \
         call, which is a successful acknowledgement"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "one effective charge, not two"
    );
    assert_eq!(upstream.count(), 1, "and no second purchase");
}

/// **The recovered charge is the price the log recorded, not the price the
/// deployment charges now.**
///
/// The restarted process is configured with a rate card ten times the original.
/// A repair that re-derived the amount would commit ten times the money, and
/// every reconciliation afterwards would be comparing a historical call against
/// a current price list. There is one pricing authority for a finished call and
/// it is the durable record.
#[tokio::test]
async fn a_recovered_charge_is_not_repriced_by_a_later_rate_card() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let original = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_reprice");
    let principal = Principal::default_open();
    let terms = original.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &original, &session).await;

    // The operator edited the rate card between the call and the recovery.
    let mut dearer = config(&base_url);
    dearer.pricing.input_per_mtok_usd = original.pricing.input_per_mtok_usd * 10.0;
    dearer.pricing.output_per_mtok_usd = original.pricing.output_per_mtok_usd * 10.0;

    let restarted = deployment(&store, &ledger, &dearer).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed, usd,
        "the historical call keeps its historical price, whatever the live card \
         says now"
    );
    assert_eq!(upstream.count(), 1);
}

/// **Repeated replay after recovery charges once and buys nothing.**
///
/// Every turn of a session replays its whole log. Once the repair is durable,
/// further turns must find nothing to do — and even before it is durable, a
/// re-driven settle deduplicates rather than accumulating.
#[tokio::test]
async fn repeated_replay_after_recovery_charges_once_and_buys_no_second_call() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_replay");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;

    let restarted = deployment(&store, &ledger, &config).await;
    drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    // A second restart, and several more turns, over a log that now records the
    // repair. Nothing here may move the money again.
    drop(restarted);
    let again = deployment(&store, &ledger, &config).await;
    for turn in ["t5", "t6", "t7"] {
        again.turn(&session, turn, "and again").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "one effective charge across arbitrarily many replays"
    );
    assert_eq!(
        repairs(&store, &session).await.len(),
        1,
        "and one durable acknowledgement, not one per turn"
    );
    assert_eq!(upstream.count(), 1, "and one HTTP purchase, ever");
    assert_eq!(
        results(&store, &session).await.len(),
        1,
        "the classification record itself is untouched by its own repair"
    );
    assert!(
        ledger.deduplicated(call_id.as_str()) || ledger.settle_calls() >= 2,
        "either the repair was the first application and later replays \
         deduplicated, or nothing re-drove it at all -- this pins which"
    );
}

/// **A call whose usage the service never reported recovers as a release, and
/// never as a measured zero cost.**
///
/// The answer arrived without a usage block. What that call billed is exactly
/// what this deployment does not know, so the settle is a zero-dollar *release*
/// of the hold — and the durable record must go on saying the accounting is
/// unknown. A recovery that recorded a measured zero would book a billed call
/// as free, which is the one accounting lie the whole module exists to prevent.
#[tokio::test]
async fn missing_usage_recovers_as_a_release_and_never_as_a_measured_zero() {
    let (base_url, upstream) = classifier_upstream(ANSWER_WITHOUT_USAGE).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_no_usage");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let first = deployment(&store, &ledger, &config).await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;
    first.turn(&session, "t2", "now add a test").await;
    drop(first);

    let before = results(&store, &session).await;
    assert_eq!(before.len(), 1);
    assert!(
        matches!(
            before[0].1.outcome.spend().expect("a call was attempted"),
            EvaluationSpend::Unknown { .. }
        ),
        "the premise: the service reported no usage, so the spend is unknown"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    let found = drive_until_repaired(&restarted, &store, &session, &["t3", "t4"]).await;

    assert_eq!(found.len(), 1, "the release is acknowledged durably too");
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "a release commits nothing -- and this zero is the ledger's, not a \
         price anybody derived"
    );
    let after = results(&store, &session).await;
    assert!(
        matches!(
            after[0].1.outcome.spend().expect("a call was attempted"),
            EvaluationSpend::Unknown { .. }
        ),
        "and the record still says the accounting is unknown: a repair \
         acknowledges a settlement, it does not invent a measurement"
    );
    assert_eq!(upstream.count(), 1);
}

/// **A charge that never landed lands in the window that is open when it is
/// repaired.**
///
/// The monthly boundary rolled between the call and its recovery. The ledger
/// applies a realized amount at the clock it is handed, so this is a *first*
/// charge and it belongs to the window it is applied in. There are no
/// historical budget buckets here and a repair must not invent one.
#[tokio::test]
async fn a_first_delayed_charge_lands_in_the_window_it_is_repaired_in() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = monthly_config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_monthly_first");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;

    // Two months on. Everything the ledger does from here is in a later window.
    ledger.skew(62 * 24 * 60 * 60 * 1_000);
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "the new window opens empty"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    let committed = drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        committed, usd,
        "a charge that had never been applied is applied now, in the window \
         that is open now"
    );
}

/// **A charge that already landed adds nothing to the window it is repaired
/// in.**
///
/// The partner of the test above, and the difference between them is the whole
/// of "distinguish a first delayed charge from a deduplicated old charge". Here
/// the settle applied in the *previous* month and only its acknowledgement was
/// lost. The settled-call record survives the reset by design, so the repair
/// deduplicates — and the new window must stay empty rather than inheriting a
/// charge that was already counted against a window that has closed.
#[tokio::test]
async fn a_deduplicated_old_charge_adds_nothing_to_the_window_it_is_repaired_in() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = monthly_config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::AfterApply);
    let session = SessionId::new("sess_monthly_dedup");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "the premise: the charge landed in this month"
    );

    ledger.skew(62 * 24 * 60 * 60 * 1_000);
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "and the window reset wiped it, which is what a window is"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    let found = drive_until_repaired(&restarted, &store, &session, &["t3", "t4"]).await;

    assert_eq!(found.len(), 1, "the acknowledgement is still recorded");
    assert!(
        ledger.deduplicated(call_id.as_str()),
        "the settled-call record outlives the window reset, so the repair is \
         recognized as the duplicate it is"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.0,
        "a deduplicated old charge must not be re-applied into a new window: \
         it was counted once, against a window that has since closed"
    );
}

/// **The repaired charge is attributed to the payer the session recorded, and
/// never to whoever happens to be serving the turn that repairs it.**
///
/// The fixture drives a later turn under a different principal. **Every
/// supported surface makes that unreachable** — `ControlPlane::Configured`
/// prefixes a session id with `{project}/{user}/` and gates every turn on
/// `contains`, and `ControlPlane::Open` resolves one fixed principal for the
/// whole deployment — so this is deliberately an out-of-contract fixture rather
/// than a scenario a deployment can produce. What it pins is the *source* of
/// the payer: a repair that read the live admission would move a finished
/// call's money to a stranger, and that is a mistake no supported caller could
/// reveal.
#[tokio::test]
async fn a_repaired_charge_is_attributed_to_the_sessions_own_payer() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_payer");
    let original = Principal::default_open();
    let stranger = Principal::new("someone", "else");
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;

    let restarted = deployment(&store, &ledger, &config).await;
    let later = Admission {
        principal: stranger.clone(),
        ..local_only()
    };
    for turn in ["t3", "t4", "t5"] {
        restarted
            .turn_as(&session, turn, "a later turn", &later)
            .await;
        for _ in 0..50 {
            if ledger.committed_usd(&original, &terms).await == usd {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    assert_eq!(
        ledger.committed_usd(&original, &terms).await,
        usd,
        "the charge lands on the principal the session's own `SessionCreated` \
         recorded"
    );
    assert_eq!(
        ledger.committed_usd(&stranger, &terms).await,
        0.0,
        "and never on the principal of the turn that happened to drive the \
         repair"
    );
}

/// **Repairing opens no grant.**
///
/// A grant under an already-settled identity creates a hold that can never
/// settle and can only expire — it would sit against the project's ceiling for
/// a whole TTL for no reason. The original call opens exactly one; recovery
/// must add none.
#[tokio::test]
async fn recovery_opens_no_second_grant() {
    let (base_url, _upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::BeforeApply);
    let session = SessionId::new("sess_no_regrant");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let (_call_id, usd) =
        a_session_with_an_unconfirmed_settlement(&store, &ledger, &config, &session).await;
    let grants_before = ledger.grant_calls();
    assert_eq!(
        grants_before, 1,
        "the premise: the original call took exactly one hold"
    );

    let restarted = deployment(&store, &ledger, &config).await;
    drive_until_committed(
        &restarted,
        &session,
        &ledger,
        &principal,
        &terms,
        usd,
        &["t3", "t4"],
    )
    .await;

    assert_eq!(
        ledger.grant_calls(),
        grants_before,
        "recovery settles an existing identity and never reserves against it \
         again"
    );
}

/// **The control: a settlement the ledger confirmed is never repaired.**
///
/// Without this, every test above would also pass against an implementation
/// that repaired *everything* on every turn — which would be the same charge
/// re-driven forever, deduplicated by the ledger and invisible in the totals.
/// A confirmed settlement must leave no repair behind at all.
#[tokio::test]
async fn control_a_confirmed_settlement_is_never_repaired() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_control");
    let principal = Principal::default_open();
    let terms = config.budget_terms();

    let first = deployment(&store, &ledger, &config).await;
    first
        .classifying_turn(&session, "t1", "fix the parser")
        .await;
    first.await_parked(&session).await;
    first.turn(&session, "t2", "now add a test").await;
    let (_call_id, usd) = recorded_measured(&store, &session).await;
    assert_eq!(
        results(&store, &session).await[0].1.outcome.committed_usd(),
        Some(usd),
        "the premise: this one settled cleanly"
    );
    let settles_after_the_call = ledger.settle_calls();
    drop(first);

    let restarted = deployment(&store, &ledger, &config).await;
    for turn in ["t3", "t4", "t5"] {
        restarted.turn(&session, turn, "and again").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        repairs(&store, &session).await.is_empty(),
        "a confirmed settlement leaves nothing to repair"
    );
    assert_eq!(
        ledger.settle_calls(),
        settles_after_the_call,
        "and the evaluation ledger is not touched again at all -- a repair pass \
         that re-drove every settled call would be invisible in the totals and \
         unbounded in traffic"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        usd,
        "one charge, unchanged"
    );
    assert_eq!(upstream.count(), 1);
}

/// **The harness control.** The ledger double's two failure modes really do
/// differ in the one way the whole suite rests on: `BeforeApply` leaves nothing
/// committed and `AfterApply` leaves the charge in place, and both hand the
/// caller an error.
///
/// If this went red, every "recovered the charge" assertion above would be
/// about the double rather than about the engine.
#[tokio::test]
async fn control_the_two_failure_modes_differ_in_what_they_leave_committed() {
    let principal = Principal::default_open();
    let terms = config("http://127.0.0.1:1").budget_terms();
    let settlement = |id: &str| Settlement {
        principal: principal.clone(),
        key: SettlementKey::OncePerCall,
        response_id: ResponseId::new(id),
        actual_usd: 3.0,
        window: terms.budget.window,
        now_ms: 1_000,
    };

    let before = RiggedLedger::new(FailMode::BeforeApply);
    assert!(before.settle_grant(settlement("call_a")).await.is_err());
    assert_eq!(
        before.committed_usd(&principal, &terms).await,
        0.0,
        "BeforeApply leaves the accounting untouched"
    );

    let after = RiggedLedger::new(FailMode::AfterApply);
    assert!(after.settle_grant(settlement("call_a")).await.is_err());
    assert_eq!(
        after.committed_usd(&principal, &terms).await,
        3.0,
        "AfterApply has already charged, and the caller cannot tell"
    );
    // And a retry of the applied one deduplicates rather than charging again.
    let retry = after
        .settle_grant(settlement("call_a"))
        .await
        .expect("a duplicate settle is a valid no-op");
    assert!(
        !retry.applied,
        "the ledger recognizes the call it already had"
    );
    assert_eq!(after.committed_usd(&principal, &terms).await, 3.0);
}

// --------------------------------------------------- durable append (claim 3)

/// A store that refuses to append a durable repair acknowledgement while
/// armed, and delegates everything else to a real [`MemoryStore`].
///
/// The engine-level partner of `RefusingStore` in `tests/classification_runtime.rs`
/// (out of scope here, so not reused directly): that double fails a
/// classification's own append; this one fails the *settlement repair's*
/// append instead — the seam
/// `Engine::deliver_settlement_repairs` names explicitly ("An acknowledgement
/// whose append fails stays with the runtime for a later turn to deliver").
struct AckRefusingStore {
    inner: MemoryStore,
    refusing: AtomicBool,
    /// Appends actually refused. Without it "no acknowledgement reached the
    /// log" is equally the signature of a deployment that never attempted one,
    /// and the whole test would pass against a repair path that did nothing.
    refusals: AtomicUsize,
}

impl AckRefusingStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            refusing: AtomicBool::new(false),
            refusals: AtomicUsize::new(0),
        })
    }

    fn refuse(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::SeqCst);
    }

    fn refusals(&self) -> usize {
        self.refusals.load(Ordering::SeqCst)
    }
}

// `Delegating` is deliberately not `use`d in this file: fixtures throughout
// call methods directly on a concrete double (`store.create_session(..)`),
// and having both traits' same-named methods in scope at once would make
// those calls ambiguous (E0034). Fully qualifying the trait here avoids that
// without pushing disambiguation onto every call site instead.
#[async_trait]
impl roundhouse_core::store::doubles::Delegating for AckRefusingStore {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
        mark: Option<roundhouse_core::store::LearningMark>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let refuses = self.refusing.load(Ordering::SeqCst)
            && kinds.iter().any(|kind| {
                matches!(
                    kind,
                    SessionEventKind::ClassificationSettlementRepaired { .. }
                )
            });
        if refuses {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            return Err(StoreError::Backend(anyhow::anyhow!(
                "the store refused this repair acknowledgement append"
            )));
        }
        self.inner.append_events(lease, kinds, mark).await
    }
}

/// **A repair acknowledgement whose durable append fails stays retryable: no
/// second purchase, no double charge, no change to what the session can
/// classify, and no repair lost.**
///
/// The failure this test drives is different from every other test in this
/// suite: those fail the *ledger's* settle; this one succeeds at the ledger
/// (the repair genuinely resolves the settlement) and fails only at writing
/// `ClassificationSettlementRepaired` into the session's own log. Per
/// `Engine::deliver_settlement_repairs`, that failure must leave the
/// acknowledgement with the runtime (`acknowledge_repairs` is only called with
/// what was actually written) rather than lost, so a later turn -- once the
/// store recovers -- delivers it exactly once.
#[tokio::test]
async fn a_failed_repair_acknowledgement_append_stays_retryable() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = AckRefusingStore::new();
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_ack_append_fails");
    let principal = Principal::default_open();
    let terms = config.budget_terms();
    let call_id = "eval_ack_append_fails";

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_ack_append_fails", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(principal.clone()),
                arm: None,
            }],
            None,
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(&store.inner, &lease, call_id, 1, 0.05).await;
    store.release_lease(&lease).await.unwrap();

    // Armed before any turn runs, so the very first repair this deployment
    // produces is the one whose append fails.
    store.refuse(true);
    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;

    // The repair itself succeeds at the ledger on this turn -- only the log
    // append of its acknowledgement is refused.
    deployment.turn(&session, "t1", "keep going").await;
    for _ in 0..200 {
        if ledger.committed_usd(&principal, &terms).await > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.05,
        "the premise: the ledger actually applied the repair"
    );
    assert!(
        repairs(&store, &session).await.is_empty(),
        "and the durable acknowledgement of it did not make it into the log -- \
         the store refused that append"
    );

    // What the session could classify with, taken while the acknowledgement is
    // still missing. Every assertion about availability below is against this
    // rather than against a count, because a repair must move neither which
    // classifications exist nor where in the log they sit.
    let availability = results(&store, &session).await;
    let calls = intents(&store, &session).await;
    assert_eq!(
        availability.len(),
        1,
        "the one classification this log holds"
    );

    // Wait for the background repair before recording the ledger call count.
    for _ in 0..200 {
        if deployment.runtime.retained_repairs(&session).await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let settle_calls_before = ledger.settle_calls();
    let repaired_at_before = deployment
        .runtime
        .ready_repairs(&session)
        .await
        .first()
        .map(|delivery| delivery.record.repaired_at_ms)
        .expect("the repair parked before any refusing turn ran");

    // Further turns while the store is still refusing must not re-drive the
    // ledger (it already applied) or buy another call.
    deployment.turn(&session, "t2", "keep going").await;
    deployment.turn(&session, "t3", "keep going").await;
    assert!(
        store.refusals() > 0,
        "the delivery path must actually have attempted the append and been \
         refused -- an empty log is otherwise equally what a deployment that \
         never tried would leave"
    );
    // Allow a background retry to reach the ledger before comparing counts.
    let mut settle_calls_after = ledger.settle_calls();
    for _ in 0..50 {
        if settle_calls_after != settle_calls_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        settle_calls_after = ledger.settle_calls();
    }
    assert_eq!(
        settle_calls_after, settle_calls_before,
        "a refused append must not read back as an eviction that re-drives \
         the ledger for a settlement it already answered"
    );
    assert_eq!(
        deployment
            .runtime
            .ready_repairs(&session)
            .await
            .first()
            .map(|delivery| delivery.record.repaired_at_ms),
        Some(repaired_at_before),
        "the same parked acknowledgement across both refusing turns, not one \
         lost and silently recreated"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.05,
        "no double charge while the append keeps failing"
    );
    assert_eq!(upstream.count(), 0, "and no classifier call, ever");

    // The store recovers. The parked acknowledgement -- never lost, because
    // `acknowledge_repairs` was never told it was written -- lands now.
    store.refuse(false);
    let found = drive_until_repaired(&deployment, &store, &session, &["t4", "t5"]).await;
    assert_eq!(
        found.len(),
        1,
        "the repair is written exactly once, once the store accepts it"
    );
    assert_eq!(
        ledger.committed_usd(&principal, &terms).await,
        0.05,
        "still one effective charge"
    );
    assert_eq!(
        results(&store, &session).await,
        availability,
        "and the classification's own availability is untouched by any of \
         this: the same records, at the same sequence numbers, so what a later \
         turn may name and when it became nameable are both unmoved"
    );
    assert_eq!(
        intents(&store, &session).await,
        calls,
        "the repair is an acknowledgement and never a new call: no intent \
         joined the log for it"
    );

    // Replays after recovery must not re-drive the already-durable repair.
    let settles_before = ledger.settle_calls();
    deployment.turn(&session, "t6", "keep going").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        ledger.settle_calls(),
        settles_before,
        "a durably acknowledged repair is not driven again"
    );
    assert_eq!(upstream.count(), 0, "a repair never reaches the classifier");
}
