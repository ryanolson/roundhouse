// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Claim 2: a second attempt at a repair that is still running -- one stuck
//! inside settle_grant, with no answer yet -- must not duplicate it, and a
//! different call proceeds while the first is held.

use super::*;

// --------------------------------------------------------- overlap (claim 2)

/// A ledger double that gates `settle_grant` for exactly one call identity and
/// passes every other call straight through.
///
/// **What this is for.** `Engine::repair_classification_settlements` skips
/// starting a repair whose call id is already in `ready_repairs` (a *parked*
/// answer). It has no way to see a repair that is still *running* — one stuck
/// inside `settle_grant`, with no answer yet. This ledger holds exactly that
/// call open so a test can drive a second turn while the first repair is
/// genuinely still in flight, and count how many concurrent `settle_grant`
/// calls the gated identity actually saw.
struct GatedRepairLedger {
    inner: MemorySpendLedger,
    gated_call: String,
    hold: AtomicBool,
    gate: tokio::sync::Notify,
    /// Fires each time the gated call enters `settle_grant`, so a test can
    /// wait for the first worker to genuinely be inside it rather than
    /// guessing with a sleep.
    entered: tokio::sync::Notify,
    concurrent: AtomicUsize,
    peak_concurrent: AtomicUsize,
    attempts: AtomicUsize,
}

impl GatedRepairLedger {
    fn new(gated_call: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            gated_call: gated_call.to_string(),
            hold: AtomicBool::new(true),
            gate: tokio::sync::Notify::new(),
            entered: tokio::sync::Notify::new(),
            concurrent: AtomicUsize::new(0),
            peak_concurrent: AtomicUsize::new(0),
            attempts: AtomicUsize::new(0),
        })
    }

    /// Let every worker currently inside `settle_grant` for the gated call
    /// proceed, and every later one pass straight through.
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.gate.notify_waiters();
    }

    /// The most `settle_grant` calls for the gated identity that were ever
    /// inside it at once. `1` is the contract; anything higher is two workers
    /// racing the same settlement.
    fn peak_concurrent(&self) -> usize {
        self.peak_concurrent.load(Ordering::SeqCst)
    }

    /// How many times `settle_grant` was entered for the gated identity,
    /// concurrent or not — the count a duplicated *acknowledgement* (not just
    /// an overlapping attempt) would also move.
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for GatedRepairLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.inner.open_grant(request).await
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        if settlement.response_id.to_string() != self.gated_call {
            return self.inner.settle_grant(settlement).await;
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_concurrent.fetch_max(now, Ordering::SeqCst);
        self.entered.notify_one();
        while self.hold.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
        let settled = self.inner.settle_grant(settlement).await;
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        settled
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.inner.balance(query).await
    }
}

/// **A repair still running must not be duplicated by a second turn, and a
/// different call's repair must not be blocked by it.**
///
/// The engine's only overlap guard reads `ready_repairs` (engine.rs,
/// `repair_classification_settlements`) and skips a call id already
/// *parked*. A repair with no answer yet is in neither `ready_repairs` nor
/// any in-flight set the engine or the runtime tracks, so a second turn
/// arriving while the first repair is still inside `settle_grant` starts a
/// second worker for the identical call — which is a duplicate ledger round
/// trip at best, and, because `ClassificationRuntime::run_repair` pushes onto
/// `repaired` unconditionally, a second durable
/// `ClassificationSettlementRepaired` event for the same call at worst.
#[tokio::test]
async fn overlapping_turns_must_not_duplicate_a_repair_still_in_flight() {
    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config(&base_url);
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new("sess_overlap");

    // Two independent unconfirmed settlements, written directly rather than
    // bought through a classifying turn: `repair_classification_settlements`
    // runs unconditionally at the end of *every* turn (engine.rs), so driving
    // this session's premise through the engine would itself race a
    // background repair worker against the next setup turn -- the very
    // overlap this test exists to control. Seeding the log directly gives the
    // restarted deployment below both settlements already unrepaired, with no
    // engine turn and therefore no repair having run yet.
    let gated_call = "eval_overlap_gated";
    let other_call = "eval_overlap_other";
    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_overlap_setup", 60_000)
        .await
        .expect("a lease read")
        .expect("an unheld session");
    store
        .append_events(
            &lease,
            vec![SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal: Some(Principal::default_open()),
                arm: None,
            }],
            None,
        )
        .await
        .unwrap();
    seed_unconfirmed_settlement(&store, &lease, gated_call, 1, 0.05).await;
    seed_unconfirmed_settlement(&store, &lease, other_call, 2, 0.07).await;
    store.release_lease(&lease).await.unwrap();

    let ledger = GatedRepairLedger::new(gated_call);
    let restarted =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;

    // t3 starts both repairs (capacity allows both); the gated one blocks
    // inside settle_grant, the other proceeds immediately.
    restarted.turn(&session, "t3", "keep going").await;
    tokio::time::timeout(Duration::from_secs(5), ledger.entered.notified())
        .await
        .expect("the repair must actually reach settle_grant to be gated at all");

    // Still stuck. A second turn arrives while nothing has parked yet.
    restarted.turn(&session, "t4", "keep going").await;

    // Give a duplicate worker, if one was started, the same chance the first
    // one had to reach the gate.
    for _ in 0..100 {
        if ledger.peak_concurrent() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        ledger.peak_concurrent(),
        1,
        "at most one worker may be inside settle_grant for the same call id \
         at once -- a second turn during the first repair's round trip must \
         not start a duplicate"
    );

    ledger.release();
    // The unblocked worker(s) finish in the background, independent of any
    // turn; wait for at least one answer to park before driving the turns
    // that deliver it, rather than racing a fixed sleep against them.
    for _ in 0..200 {
        if !restarted.runtime.ready_repairs(&session).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let found = drive_until_repaired(&restarted, &store, &session, &["t5", "t6"]).await;
    let gated_repairs = found
        .iter()
        .filter(|value| value["record"]["call_id"] == gated_call)
        .count();
    assert_eq!(
        gated_repairs, 1,
        "one durable acknowledgement per call, not one per duplicate worker"
    );
    assert_eq!(
        ledger.attempts(),
        1,
        "and the ledger was asked about this call exactly once"
    );

    // The other call must not have been starved by the gate.
    let other_found = drive_until_repaired_for(
        &restarted,
        &store,
        &session,
        &ResponseId::new(other_call),
        &["t7", "t8"],
    )
    .await;
    assert_eq!(
        other_found, 1,
        "a different call's repair must still progress while the gated one \
         is held"
    );
    assert_eq!(
        upstream.count(),
        0,
        "a repair never reaches the classifier -- there is no answer to buy"
    );
}
