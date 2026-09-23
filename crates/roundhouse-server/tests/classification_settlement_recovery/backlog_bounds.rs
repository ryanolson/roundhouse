// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounding what one turn's own repair scheduling costs against a long
//! outage backlog -- a bounded rate per turn (claim 4), and the same bound
//! under a ledger that fails every attempt immediately rather than one
//! deferred failure per call.

use super::*;

// -------------------------------------------------- backlog scheduling (claim 4)

/// As [`config`], with a caller-chosen `max_in_flight` — so a backlog test can
/// make the admission ceiling small enough to observe without waiting out a
/// large one.
fn config_with_max_in_flight(base_url: &str, max_in_flight: usize) -> ClassifyConfig {
    let mut config = config(base_url);
    config.executor.max_in_flight = max_in_flight;
    config
}

/// **A long outage backlog is drained at a bounded rate per turn, not all at
/// once.**
///
/// Twenty unconfirmed settlements in one session -- far more than the
/// `max_in_flight` of 3 this deployment runs with -- seeded directly (see
/// [`seed_unconfirmed_settlement`] for why not through the engine). One turn
/// must not start more repairs than the admission ceiling allows; the rest
/// stay in the log for a later turn, which is what keeps a recovering
/// deployment's own serving turns cheap regardless of how large the backlog
/// that produced them was.
///
/// The sibling claim -- that a turn does not copy the whole backlog to decide
/// what to schedule -- is
/// [`a_long_outage_backlog_bounds_what_a_turn_retains_and_copies`].
#[tokio::test]
async fn a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn() {
    const BACKLOG: u64 = 20;
    const MAX_IN_FLIGHT: usize = 3;

    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config_with_max_in_flight(&base_url, MAX_IN_FLIGHT);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_backlog");
    let principal = Principal::default_open();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_backlog_setup", 60_000)
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
    for i in 1..=BACKLOG {
        seed_unconfirmed_settlement(&store, &lease, &format!("eval_backlog_{i}"), i, 0.01).await;
    }
    store.release_lease(&lease).await.unwrap();

    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    assert_eq!(
        results(&store, &session).await.len(),
        BACKLOG as usize,
        "the premise: the whole backlog is durably unconfirmed before any turn \
         runs"
    );

    deployment.turn(&session, "t1", "keep going").await;
    // The turn itself only spawns workers; give them a bounded window to
    // actually reach the ledger before reading how many did.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settled_by_t1 = ledger.settle_calls();
    assert!(
        settled_by_t1 <= MAX_IN_FLIGHT,
        "one turn must start at most `max_in_flight` repairs against a \
         backlog far larger than that -- got {settled_by_t1} settle_grant \
         calls after a single turn with a ceiling of {MAX_IN_FLIGHT}"
    );
    assert!(
        settled_by_t1 > 0,
        "and it must start at least one -- an outage backlog that never \
         drains is a different defect this assertion would otherwise hide"
    );

    // A second turn, once the first batch's acknowledgements have had a
    // chance to land, admits roughly another `max_in_flight` worth of work --
    // bounded progress, not a stall and not the rest of the backlog at once.
    drive_until_repaired(&deployment, &store, &session, &["t2"]).await;
    deployment.turn(&session, "t3", "keep going").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settled_by_t3 = ledger.settle_calls();
    assert!(
        settled_by_t3 <= 3 * MAX_IN_FLIGHT,
        "three turns must not have exceeded three turns' worth of admitted \
         repairs -- got {settled_by_t3} settle_grant calls against a ceiling \
         of {MAX_IN_FLIGHT} per turn"
    );
    assert!(
        settled_by_t3 > settled_by_t1,
        "and real progress must still be happening on the third turn"
    );
    assert_eq!(upstream.count(), 0, "a repair never reaches the classifier");
}

/// **What a turn retains, and what it copies, are both bounded by admission --
/// not by the size of the backlog the outage left.**
///
/// The partner of the test above, and the half it could not reach. That one
/// bounds how many repairs a turn *starts*; this one bounds the state they
/// leave behind and the work a later turn does over it. Both are the same
/// property in the end: an acknowledgement holds its permit until it is
/// committed, so `max_in_flight` caps how many can be outstanding at once, and
/// the scheduling path no longer reads them at all -- a turn takes one copy of
/// what it is about to write and none to decide what to start.
///
/// Measured through `ClassificationRuntime::repair_handles_issued`, a counter
/// of every retained acknowledgement handed to a turn, rather than through a
/// clock: a wall-time assertion on this box would be about the box.
///
/// The last assertion is the one that keeps the first two honest -- a backlog
/// that never drains would satisfy every bound here perfectly.
#[tokio::test]
async fn a_long_outage_backlog_bounds_what_a_turn_retains_and_copies() {
    const BACKLOG: u64 = 20;
    const MAX_IN_FLIGHT: usize = 3;

    let (base_url, upstream) = classifier_upstream(ANSWER).await;
    let config = config_with_max_in_flight(&base_url, MAX_IN_FLIGHT);
    let store = Arc::new(MemoryStore::new());
    let ledger = RiggedLedger::new(FailMode::Never);
    let session = SessionId::new("sess_backlog_bounds");
    let principal = Principal::default_open();

    store.create_session(&session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(&session, "node_backlog_bounds_setup", 60_000)
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
    for i in 1..=BACKLOG {
        seed_unconfirmed_settlement(&store, &lease, &format!("eval_bounds_{i}"), i, 0.01).await;
    }
    store.release_lease(&lease).await.unwrap();

    let deployment =
        deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
    deployment.turn(&session, "t1", "keep going").await;

    // A window in which an unbounded implementation parks the whole backlog --
    // every repair this ledger answers immediately -- rather than a sleep
    // chosen to be long enough for a bound that already holds.
    let mut retained = 0;
    for _ in 0..100 {
        retained = retained.max(deployment.runtime.retained_repairs(&session).await);
        if retained > MAX_IN_FLIGHT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        retained <= MAX_IN_FLIGHT,
        "an acknowledgement nobody has committed is outstanding work, so a \
         backlog of {BACKLOG} must not leave more than the ceiling of \
         {MAX_IN_FLIGHT} of them retained at once -- saw {retained}"
    );
    assert!(
        retained > 0,
        "and the turn must actually have repaired something for this to be \
         about a bound rather than about an idle deployment"
    );

    // One turn's worth of copying, measured across exactly one turn.
    let copied_before = deployment.runtime.repair_handles_issued();
    deployment.turn(&session, "t2", "keep going").await;
    let copied = deployment.runtime.repair_handles_issued() - copied_before;
    assert!(
        copied <= MAX_IN_FLIGHT,
        "a turn copies the acknowledgements it is about to write and nothing \
         else -- {copied} entries were handed to one turn against a ceiling of \
         {MAX_IN_FLIGHT}"
    );

    // And the backlog really drains, a bounded batch at a time.
    let mut turns = 0;
    while repairs(&store, &session).await.len() < BACKLOG as usize && turns < 60 {
        deployment
            .turn(&session, &format!("t_drain_{turns}"), "keep going")
            .await;
        turns += 1;
    }
    assert_eq!(
        repairs(&store, &session).await.len(),
        BACKLOG as usize,
        "every settlement is acknowledged exactly once after {turns} turns -- \
         neither the admission ceiling nor the identity claim may strand one"
    );
    assert_eq!(upstream.count(), 0, "a repair never reaches the classifier");
}

// --------------------------------------- fast-failure scheduling (claim 1)

/// An evaluation ledger whose every settle fails at once -- no gate, no
/// deferred single failure, the shape a backend that is simply *down*
/// produces, as opposed to [`RiggedLedger`]'s one deferred failure per call
/// id or [`GatedRepairLedger`]'s held-open round trip. Counts every attempt.
struct FastFailingLedger {
    attempts: AtomicUsize,
}

impl FastFailingLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            attempts: AtomicUsize::new(0),
        })
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SpendLedger for FastFailingLedger {
    async fn open_grant(&self, _request: GrantRequest) -> Result<Grant, SpendError> {
        unreachable!("a repair settles an existing hold and never opens one")
    }

    async fn settle_grant(&self, _settlement: Settlement) -> Result<Settled, SpendError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(SpendError::Backend(anyhow::anyhow!(
            "simulated ledger outage: every settle fails immediately"
        )))
    }

    async fn balance(&self, _query: BalanceQuery) -> Result<Balance, SpendError> {
        unreachable!("a repair reads no balance")
    }
}

/// Seed `backlog` independent unconfirmed settlements directly into a fresh
/// session's log, on the same rationale [`a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn`]
/// seeds directly rather than through the engine: `repair_classification_settlements`
/// runs at the end of every turn, including a setup turn, so building the
/// backlog through the engine would start repairing it before this test's
/// own turn does.
async fn seed_backlog(
    store: &MemoryStore,
    session: &SessionId,
    principal: &Principal,
    prefix: &str,
    backlog: u64,
) {
    store.create_session(session, "affinity").await.unwrap();
    let lease = store
        .acquire_lease(session, &format!("node_{prefix}_setup"), 60_000)
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
    for i in 1..=backlog {
        seed_unconfirmed_settlement(store, &lease, &format!("eval_{prefix}_{i}"), i, 0.01).await;
    }
    store.release_lease(&lease).await.unwrap();
}

/// **Repair scheduling attempts within one engine turn, when every ledger
/// settle fails immediately, for two backlogs two orders of magnitude
/// apart.**
///
/// [`a_long_outage_backlog_is_drained_at_a_bounded_rate_per_turn`] proves the
/// bound holds when every repair *succeeds*: a completed repair parks its
/// acknowledgement and keeps its permit until a later turn commits it, so
/// nothing can recycle capacity inside that one turn's own scheduling loop --
/// the retention and delivery-handle counters that test and its sibling read
/// are both downstream of that same parked-and-held state. A *failed* repair
/// takes a different path: `ClassificationRuntime::run_repair` returns with
/// nothing parked on a ledger error, which drops both `Capacity` and the
/// identity claim the instant that worker's future ends, with no later turn
/// required to free either. Under a multi-threaded runtime and a ledger that
/// answers every settle instantly, a permit freed that way can be
/// re-acquired by the same turn's still-running `repair_classification_settlements`
/// loop before that loop has finished walking the rest of the backlog -- so
/// whether one turn's attempts stay at `max_in_flight` or run past it is a
/// scheduling question the admission semaphore alone does not decide. This
/// measures it rather than assuming either answer, over backlogs two orders
/// of magnitude apart so a size-dependent effect would show up as a
/// difference between them rather than being invisible at one size.
///
/// Measured through the ledger's own `settle_grant` entry count, which is a
/// 1:1 count of repair *attempts* here: every seeded call id is unique, so no
/// attempt is ever refused as a same-identity duplicate, and every attempt
/// that is not refused calls `settle_grant` exactly once
/// (`TypeSafeShadow::repair_settlement`). Counted only after the one turn
/// that started them has returned and the running total has stopped moving,
/// so this is work attributable to that turn's own scheduling loop and not to
/// a later drain -- nothing else in this test ever calls `deployment.turn`
/// again for either backlog.
///
/// **What makes the two assertions below a guarantee rather than an
/// observation.** This test measured the question and could not settle it: a
/// run that stayed at `max_in_flight` said only that this box had scheduled
/// that way. `ClassificationRuntime::repair_batch` now fixes one turn's
/// candidates before the loop starts, so the ceiling holds however the workers
/// interleave, and
/// `classify_runtime::tests::a_turn_considers_at_most_max_in_flight_settlements_however_large_the_backlog`
/// is the structural half of it. This test stays as the runtime half: it is
/// what proves the bound survives a real engine turn, real workers and a
/// ledger that recycles permits as fast as it can fail.
#[tokio::test(flavor = "multi_thread")]
async fn repair_scheduling_attempts_per_turn_under_fast_ledger_failures() {
    const MAX_IN_FLIGHT: usize = 3;

    async fn attempts_after_one_turn(backlog: u64, label: &str) -> usize {
        let (base_url, upstream) = classifier_upstream(ANSWER).await;
        let config = config_with_max_in_flight(&base_url, MAX_IN_FLIGHT);
        let store = Arc::new(MemoryStore::new());
        let ledger = FastFailingLedger::new();
        let session = SessionId::new(format!("sess_fastfail_{label}"));
        let principal = Principal::default_open();

        seed_backlog(&store, &session, &principal, label, backlog).await;

        let deployment =
            deployment_over(&store, Arc::clone(&ledger) as Arc<dyn SpendLedger>, &config).await;
        deployment.turn(&session, "t1", "keep going").await;

        // A plateau wait rather than a fixed sleep: workers fail near
        // instantly, so poll until the running total stops moving (or a
        // generous ceiling elapses) instead of guessing a duration.
        let mut last = ledger.attempts();
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let now = ledger.attempts();
            if now == last {
                break;
            }
            last = now;
        }
        let attempts = ledger.attempts();
        assert_eq!(
            upstream.count(),
            0,
            "a repair never reaches the classifier, backlog={backlog}"
        );
        assert!(
            attempts > 0,
            "the turn must have started at least one repair for this to be \
             about a bound rather than about an idle deployment, \
             backlog={backlog}"
        );
        assert!(
            attempts <= backlog as usize,
            "sanity ceiling: one turn cannot attempt more repairs than the \
             backlog contains, backlog={backlog}, attempts={attempts}"
        );
        attempts
    }

    let small = attempts_after_one_turn(20, "small").await;
    let large = attempts_after_one_turn(2000, "large").await;

    eprintln!(
        "repair_scheduling_attempts_per_turn_under_fast_ledger_failures: \
         max_in_flight={MAX_IN_FLIGHT}, backlog=20 -> {small} attempts, \
         backlog=2000 -> {large} attempts"
    );

    assert!(
        small <= MAX_IN_FLIGHT,
        "one turn attempted {small} repairs against a backlog of 20 and a \
         ceiling of {MAX_IN_FLIGHT} -- fast ledger failures let this turn's \
         own scheduling loop recycle capacity within itself"
    );
    assert!(
        large <= MAX_IN_FLIGHT,
        "one turn attempted {large} repairs against a backlog of 2000 and a \
         ceiling of {MAX_IN_FLIGHT} -- fast ledger failures let this turn's \
         own scheduling loop recycle capacity within itself"
    );
}
