// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Background classification with delivery through a later turn's writer.
//!
//! An admission permit covers the prompt capture, queued work, execution, and
//! the retained result — and, for a settlement repair, the ledger round trip
//! and the acknowledgement it parks. The engine takes it before it copies
//! anything out of the turn's items, so a saturated or stopped runtime costs
//! the serving turn no payload it cannot submit; delivery handles then share
//! that permit, so eviction cannot release capacity while another handle still
//! retains the record. A separate semaphore limits concurrent HTTP calls.
//! Saturation skips classification without failing the serving turn.
//!
//! One absolute expiry, taken at submission, covers every wait a call can make:
//! the queue, the budget grant, the HTTP round trip, and the settlement that
//! follows it. Each phase gets what is left of that instant rather than a fresh
//! duration of its own, so a call cannot park in a ledger round trip holding an
//! admission permit it can never release. Result retention is the one clock that
//! is separate, and it starts at completion so a slow call still has time for
//! delivery. The supervisor sweeps idle-session results and collects finished
//! worker handles without requiring another turn.
//!
//! The engine records the intent before a worker can send HTTP. An unanswered
//! intent leaves the answer and cost unknown. Replay must not send it again:
//! the first request could already have reached the provider.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use roundhouse_core::classify::projection::PromptCapture;
use roundhouse_core::classify::{
    AvailableClassification, ClassificationOutcome, ClassificationRecord,
    ClassificationSettlementRepair, FundingRefusal, PriorTurnMetadata, UnconfirmedSettlement,
};
use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{BudgetTerms, Principal, SpendLedger, TurnCredential};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::routing::Target;
use roundhouse_fleet::typesafe::SystemOneClient;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::classify_config::ClassifyConfig;
use crate::typesafe_shadow::{NotRun, PreparedCall, ShadowCall, TypeSafeShadow};

/// The bounds one deployment runs background classification under.
///
/// No [`Default`]: every field is a decision about how much of this process a
/// deployment is willing to spend on work nobody is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLimits {
    /// Classifications queued, running, or completed and undelivered, together.
    pub max_in_flight: usize,
    /// Calls on the wire at once. Separate from the above on purpose.
    pub max_http_concurrency: usize,
    /// How long a call may live from submission, across the queue and the wire.
    pub call_ttl_ms: u64,
    /// How long a finished result remains available for delivery.
    /// This interval starts at completion rather than call expiry, so a slow
    /// call does not lose its opportunity for delivery.
    pub result_retention_ms: u64,
    /// How often the supervisor reclaims expired results and finished workers.
    pub sweep_interval_ms: u64,
}

/// Everything about the turn being classified that [`ClassificationRuntime::prepare`]
/// needs and cannot derive from the projection or the decision.
///
/// `call_id` is the caller's to mint rather than the runtime's, because the
/// two callers want different things from it: the engine generates a fresh
/// id per external attempt — a settled identity can never settle again, so
/// reusing the turn's own would collide with its hold — while a test names
/// one explicitly to assert against later.
pub struct ClassificationSource {
    pub principal: Principal,
    pub session_id: SessionId,
    pub call_id: ResponseId,
    pub source_turn_index: u64,
    pub source_response_id: ResponseId,
}

/// An admission permit transferred from the caller to its worker and result.
pub struct Capacity(OwnedSemaphorePermit);

/// One repair attempt's identity: the session whose log holds the settlement,
/// and the original call that settlement belongs to.
type RepairIdentity = (SessionId, ResponseId);

/// The exclusive claim on one repair identity.
///
/// **Released by `Drop`, which is the point.** A repair ends at its deadline,
/// at a ledger error, or by being cancelled mid-await, and a claim released by
/// an explicit call would leak on whichever of those three a later edit forgot
/// — pinning a settlement as forever in-flight, so no later turn would ever
/// retry it. Dropping the worker's future releases this exactly as it releases
/// the admission permit beside it.
struct RepairClaim {
    held: Arc<std::sync::Mutex<HashSet<RepairIdentity>>>,
    identity: RepairIdentity,
}

impl Drop for RepairClaim {
    fn drop(&mut self) {
        self.held
            .lock()
            // Nothing under this lock can panic, so poison is unreachable; a
            // `Drop` that unwrapped would turn an impossible state into an
            // abort during unwinding all the same.
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.identity);
    }
}

/// What a [`Mailbox`] needs from its record to acknowledge one by identity.
pub(crate) trait ParkedRecord {
    fn call_id(&self) -> &ResponseId;
}

impl ParkedRecord for ClassificationRecord {
    fn call_id(&self) -> &ResponseId {
        &self.call_id
    }
}

impl ParkedRecord for ClassificationSettlementRepair {
    fn call_id(&self) -> &ResponseId {
        &self.call_id
    }
}

/// One piece of background work, complete and held for a turn's writer to
/// deliver — a classification result or a settlement-repair acknowledgement,
/// the same shape either way.
///
/// **The permit is a field.** That is the whole of "a completed-but-undelivered
/// record still occupies capacity": the record is reachable through an `Arc`,
/// so the map and every delivery handle share one permit, and capacity returns
/// only when the last of them is dropped.
///
/// **`_claim` is `Some` only for a repair.** An acknowledgement nobody has
/// written yet is outstanding work by the same definition an undelivered
/// result is, so it occupies `max_in_flight` until it is delivered or swept —
/// and, uniquely to a repair, the settlement's identity stays claimed for
/// exactly as long, so the turn that still reads it as unrepaired in the log
/// does not start a second attempt at a question that is already answered. A
/// result protects no identity from a second attempt: a classification call
/// is never retried once it has an answer.
pub struct Parked<R> {
    pub record: R,
    /// When the sweep may reclaim this, counted from *completion* (or, for a
    /// repair, from the ledger's answer) and not from the call's own expiry.
    /// See [`Mailbox::park`].
    retain_until_ms: u64,
    _permit: OwnedSemaphorePermit,
    _claim: Option<RepairClaim>,
}

impl<R> Parked<R> {
    /// When this record stops being held for delivery. No production reader:
    /// the sweep compares the field directly, and this accessor exists so a
    /// test can assert the same clock without reaching into the struct.
    #[cfg(any(test, feature = "test-support"))]
    pub fn retain_until_ms(&self) -> u64 {
        self.retain_until_ms
    }
}

/// A handle on one parked record.
///
/// Cloning it is cheap and — deliberately — does not release anything: the
/// permit and (for a repair) the claim live until the last handle drops, so
/// an entry the sweep evicted while a turn was writing it still bounds
/// admission.
pub type Delivered<R> = Arc<Parked<R>>;

/// Completed background work, held per session until a turn's writer takes
/// it or the sweep reclaims it.
///
/// **One mailbox type for results and for repairs**, in place of two
/// hand-duplicated maps: append-then-acknowledge, retention from completion,
/// and eviction-safe permits are one idea, not two. The two instances the
/// runtime holds ([`ClassificationRuntime::results`] and
/// [`ClassificationRuntime::repairs`]) differ only in what `R` is and in
/// whether a park carries a [`RepairClaim`].
struct Mailbox<R> {
    parked: Mutex<HashMap<SessionId, Vec<Delivered<R>>>>,
}

impl<R: ParkedRecord> Mailbox<R> {
    fn new() -> Self {
        Self {
            parked: Mutex::new(HashMap::new()),
        }
    }

    /// Hold a finished record until a turn with a writer takes it.
    ///
    /// **Retention is its own clock, starting at `retain_until_ms`.** It used
    /// to be the call's absolute expiry, and that was a defect the
    /// hypothesis list named: a call that finished in the last second of its
    /// life was swept before any turn could drain it, so every
    /// slow-but-successful classification was thrown away. The call expiry
    /// bounds *making* the call; this bounds *holding* its answer.
    async fn park(
        &self,
        session_id: SessionId,
        record: R,
        retain_until_ms: u64,
        permit: OwnedSemaphorePermit,
        claim: Option<RepairClaim>,
    ) {
        let entry = Arc::new(Parked {
            record,
            retain_until_ms,
            _permit: permit,
            _claim: claim,
        });
        self.parked
            .lock()
            .await
            .entry(session_id)
            .or_default()
            .push(entry);
    }

    /// What this session has waiting, without giving it up.
    ///
    /// **Cloned handles, and the entries stay.** Removing them here would lose
    /// every record whose append then failed; the caller acknowledges what it
    /// committed, and the log's own record of a delivered call is what stops a
    /// re-offer becoming a duplicate.
    async fn ready(&self, session_id: &SessionId) -> Vec<Delivered<R>> {
        self.parked
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Drop what a writer committed.
    async fn acknowledge(&self, session_id: &SessionId, ids: &[ResponseId]) {
        if ids.is_empty() {
            return;
        }
        let mut parked = self.parked.lock().await;
        let Some(waiting) = parked.get_mut(session_id) else {
            return;
        };
        waiting.retain(|entry| !ids.contains(entry.record.call_id()));
        if waiting.is_empty() {
            parked.remove(session_id);
        }
    }

    /// How many records this session is holding. Reads the length rather
    /// than taking handles, so a test can watch retention without moving a
    /// side-effecting counter (like [`ClassificationRuntime::repair_handles_issued`])
    /// by looking.
    #[cfg(any(test, feature = "test-support"))]
    async fn retained(&self, session_id: &SessionId) -> usize {
        self.parked.lock().await.get(session_id).map_or(0, Vec::len)
    }

    /// Reclaim records nobody came back for. Returns how many it dropped.
    async fn sweep(&self, now_ms: u64) -> usize {
        let mut dropped = 0;
        self.parked.lock().await.retain(|_, waiting| {
            let before = waiting.len();
            waiting.retain(|entry| entry.retain_until_ms > now_ms);
            dropped += before - waiting.len();
            !waiting.is_empty()
        });
        dropped
    }

    /// Drop everything parked. [`ClassificationRuntime::shutdown`]'s own
    /// gate covers this: no production path calls it either.
    #[cfg(any(test, feature = "test-support"))]
    async fn clear(&self) {
        self.parked.lock().await.clear();
    }
}

/// The background executor, and the policy boundary it drives.
pub struct ClassificationRuntime<T: Tokenizer> {
    shadow: Arc<TypeSafeShadow<T>>,
    /// The evaluation budget every call is held against, applied per principal
    /// so a project's and a member's ceilings both bind.
    terms: BudgetTerms,
    /// This deployment's own key for the classifier. Never a caller's.
    credential: TurnCredential,
    limits: RuntimeLimits,
    admission: Arc<Semaphore>,
    http: Arc<Semaphore>,
    /// Completed classifications by session, awaiting a turn with a writer.
    results: Mailbox<ClassificationRecord>,
    /// Settlement repairs the ledger has answered, awaiting the same writer.
    ///
    /// A second [`Mailbox`] rather than a shared one, because the two carry
    /// different record types: both hold the admission permit their work was
    /// admitted under until a turn takes them, so a session with no next turn
    /// cannot accumulate either beyond `max_in_flight`. Swept on the same
    /// retention clock, and **dropping one is safe rather than merely
    /// cheap**: the log still records the settlement as unrepaired, so a
    /// later turn re-drives it, the ledger deduplicates, and the
    /// acknowledgement is written then. What a sweep costs is one redundant
    /// ledger call, never a second charge and never a classifier request.
    repairs: Mailbox<ClassificationSettlementRepair>,
    /// Repair identities some attempt is holding — running, or answered and
    /// not yet committed. See [`RepairClaim`].
    ///
    /// **A `std` lock, and that is what makes `Drop` the release path.** A
    /// cancelled worker's future is dropped by whoever dropped it, often with
    /// no runtime to await on, so a release that had to lock asynchronously
    /// could not run there at all. Every critical section is one set
    /// operation.
    repair_claims: Arc<std::sync::Mutex<HashSet<RepairIdentity>>>,
    /// Retained acknowledgements handed to a turn since this runtime started.
    ///
    /// A gauge production never reads: [`Self::ready_repairs`] is the only path
    /// that copies retained repair state onto a serving turn, so this is what
    /// a test measures per-turn work against. Counting here rather than timing
    /// it is what keeps that test about the bound instead of about the box.
    #[cfg(any(test, feature = "test-support"))]
    repair_handles_issued: AtomicUsize,
    workers: Mutex<JoinSet<()>>,
    /// Raised once, when this runtime's lifetime ends.
    ///
    /// **A signal a worker selects on, rather than a join.** The only thing
    /// that ends the lifetime in production is [`Supervisor::drop`], and `Drop`
    /// cannot await — so cancellation has to cost no lock and no blocking
    /// cleanup. A permit comes back when the cancelled worker's future is
    /// dropped, which needs nothing from the dropper at all.
    cancel: tokio::sync::watch::Sender<bool>,
}

impl<T: Tokenizer> ClassificationRuntime<T> {
    /// End this runtime's lifetime: no new admission, and every worker still in
    /// flight is cancelled where it stands.
    ///
    /// **Synchronous, so the production lifetime guard can call it.** Nothing
    /// here waits on a task or takes a lock, which is what keeps wind-down off
    /// the path of a process that is already shutting down.
    ///
    /// **A cancelled call records nothing**, and that is the honest answer
    /// rather than a missing one: its request may already have reached the
    /// service, so its intent stays unanswered and reads as *unknown*. An
    /// [`ClassificationOutcome::Unfunded`] result would say the opposite — that
    /// this deployment knows the call cost nothing.
    pub fn stop(&self) {
        self.cancel.send_replace(true);
    }
}

impl<T: Tokenizer + Send + Sync + 'static> ClassificationRuntime<T> {
    pub fn new(
        shadow: TypeSafeShadow<T>,
        terms: BudgetTerms,
        credential: TurnCredential,
        limits: RuntimeLimits,
    ) -> Self {
        Self {
            shadow: Arc::new(shadow),
            terms,
            credential,
            limits,
            admission: Arc::new(Semaphore::new(limits.max_in_flight)),
            http: Arc::new(Semaphore::new(limits.max_http_concurrency)),
            results: Mailbox::new(),
            repairs: Mailbox::new(),
            repair_claims: Arc::new(std::sync::Mutex::new(HashSet::new())),
            #[cfg(any(test, feature = "test-support"))]
            repair_handles_issued: AtomicUsize::new(0),
            workers: Mutex::new(JoinSet::new()),
            // The receiver is taken per worker through `subscribe`, so the one
            // the channel hands back here has nobody to read it.
            cancel: tokio::sync::watch::channel(false).0,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn limits(&self) -> RuntimeLimits {
        self.limits
    }

    /// What may be captured from a turn's input, so the engine can bound the
    /// capture at the moment it holds the items rather than afterwards.
    pub fn projection_caps(&self) -> roundhouse_core::classify::ProjectionCaps {
        self.shadow.config().caps
    }

    /// Room for one more, or nothing. **Never blocks.**
    ///
    /// Called from the turn path *before* the turn's prompt is copied, so a
    /// queue that is full has to answer immediately: waiting here would put a
    /// background bound on the latency of a turn that is still serving. What
    /// the permit admits is the payload built under it, which is why it is
    /// asked for first and carried to [`Self::spawn`] rather than taken again
    /// once the payload exists.
    pub fn capacity(&self) -> Option<Capacity> {
        if *self.cancel.borrow() {
            return None;
        }
        Arc::clone(&self.admission)
            .try_acquire_owned()
            .ok()
            .map(Capacity)
    }

    /// Permits available right now. No production caller: an operator's gauge
    /// would read this, but none is wired up yet, and the only readers today
    /// are tests asserting a permit was taken or given back.
    #[cfg(any(test, feature = "test-support"))]
    pub fn available_capacity(&self) -> usize {
        self.admission.available_permits()
    }

    /// Render the bounded projection and prepare the request over it, or
    /// answer why this turn cannot be classified.
    ///
    /// **One call rather than two.** `projection` and `prepare` used to be
    /// separate pass-throughs to [`TypeSafeShadow`], and the engine's only
    /// caller always ran them back to back — the projection has no other use
    /// than feeding straight into `prepare`, so the seam bought nothing but a
    /// second place for the ordering to be gotten wrong.
    pub fn prepare(
        &self,
        source: ClassificationSource,
        capture: &PromptCapture,
        prior: &[AvailableClassification],
        local: &[PriorTurnMetadata],
        admitted: Option<&[Target]>,
        now_ms: u64,
    ) -> Result<PreparedCall, NotRun> {
        let projection = self.shadow.projection(capture, prior, local)?;
        self.shadow.prepare(
            ShadowCall {
                principal: source.principal,
                session_id: source.session_id,
                call_id: source.call_id,
                source_turn_index: source.source_turn_index,
                source_response_id: source.source_response_id,
                terms: self.terms.clone(),
                credential: &self.credential,
                now_ms,
                expires_at_ms: now_ms.saturating_add(self.limits.call_ttl_ms),
            },
            &projection,
            admitted,
        )
    }

    /// Run one prepared call, whose intent is already durable.
    ///
    /// **Refuses after stopping**, and not only at the capacity check: a permit
    /// taken before the lifetime ended and spent after it would start an
    /// external call against a runtime that has already stopped, which is the
    /// one thing stopping is for.
    ///
    /// A worker that *did* start is cancelled by [`Self::stop`] rather than
    /// left to run its deadline out against an upstream nobody is waiting for.
    /// Cancelling cannot un-send a request already on the wire, which is
    /// exactly why the cancelled call records nothing at all rather than a call
    /// that cost nothing.
    pub async fn spawn(
        self: &Arc<Self>,
        capacity: Capacity,
        session_id: SessionId,
        call: PreparedCall,
    ) {
        if *self.cancel.borrow() {
            return;
        }
        let runtime = Arc::clone(self);
        self.spawn_cancellable(async move { runtime.run(capacity, session_id, call).await })
            .await;
    }

    /// The worker body. Holds no session lease and opens no writer.
    async fn run(self: Arc<Self>, capacity: Capacity, session_id: SessionId, call: PreparedCall) {
        let expires_at_ms = call.expires_at_ms();
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(
                expires_at_ms.saturating_sub(roundhouse_core::now_ms()),
            );
        // The queue wait spends the same expiry the call does. A permit that
        // arrives after the deadline buys a request nobody may still want.
        let http = tokio::time::timeout_at(deadline, Arc::clone(&self.http).acquire_owned()).await;
        // **Checked again against the clock, and not only through the timeout.**
        // `timeout_at` polls its inner future before it looks at the deadline, so
        // an uncontended permit — the ordinary case — resolves immediately and a
        // call whose life had already run out would be sent anyway. The timeout
        // bounds the *wait*; this bounds the call.
        let expired = roundhouse_core::now_ms() >= expires_at_ms || *self.cancel.borrow();
        let Ok(Ok(_http)) = http else {
            self.park_unfunded(session_id, call, FundingRefusal::Expired, capacity)
                .await;
            return;
        };
        if expired {
            self.park_unfunded(session_id, call, FundingRefusal::Expired, capacity)
                .await;
            return;
        }
        // The same instant the queue wait was bounded by, handed to everything
        // that follows it: a call that starts with two seconds of its life left
        // gets two seconds across its grant, its request and its settle, not a
        // fresh deadline for each.
        let record = self.shadow.execute(call, deadline).await;
        self.park(session_id, record, capacity).await;
    }

    /// Re-drive one unconfirmed settlement on a background worker.
    ///
    /// **Bounded, off the serving path, and holding no writer.** It takes an
    /// admission permit like a classification does, so a deployment whose
    /// ledger is down cannot accumulate repair workers without bound; it runs
    /// on the executor rather than on the turn, so no response waits for a
    /// ledger round trip; and it parks its answer for a later turn's writer
    /// rather than opening a session of its own, which would fence the turn
    /// that is running.
    ///
    /// **Its own bound, starting now.** `call_ttl_ms` measured from this
    /// instant, deliberately not the original call's absolute expiry — that
    /// instant is in the past, often by days, so binding against it would make
    /// every repair time out before it began. This does not extend the call it
    /// settles: that call is long over, and what is left is accounting.
    ///
    /// **No HTTP, ever.** There is no answer to buy here. A repair that failed
    /// and is driven again by a later turn costs one more deduplicated ledger
    /// call and not a cent.
    ///
    /// **One attempt per settlement identity at a time.** The log goes on
    /// saying a settlement is unrepaired until an acknowledgement is committed,
    /// which is two turns after the worker started in the ordinary case — so
    /// without a claim held across the ledger round trip *and* the parked
    /// answer, every turn in that window starts another worker for the same
    /// call. The money survives that (the ledger deduplicates by call
    /// identity), the work does not: duplicate round trips against a ledger
    /// that is often recovering from the outage that caused the backlog, and a
    /// second durable acknowledgement for one call. A duplicate is refused
    /// here rather than at the caller so the check and the claim are one
    /// operation; `capacity` goes back with this return, so a refused
    /// duplicate costs an unrelated settlement nothing.
    pub async fn repair(
        self: &Arc<Self>,
        capacity: Capacity,
        session_id: SessionId,
        principal: Principal,
        settlement: UnconfirmedSettlement,
    ) {
        if *self.cancel.borrow() {
            return;
        }
        let Some(claim) = self.claim_repair(&session_id, &settlement.call_id) else {
            return;
        };
        let runtime = Arc::clone(self);
        self.spawn_cancellable(async move {
            runtime
                .run_repair(capacity, claim, session_id, principal, settlement)
                .await
        })
        .await;
    }

    /// Spawn `work` on the executor, cancelled the instant this runtime's
    /// lifetime ends.
    ///
    /// **The `subscribe` + `select!` pattern, owned once.** [`Self::spawn`]
    /// and [`Self::repair`] each used to repeat it, and a copy that drifted
    /// would be a worker that either raced its own cancellation check or
    /// missed it. Subscribed here, before the task exists, so a `stop`
    /// landing between this call and the task's first poll is still seen:
    /// `wait_for` answers on the value the receiver already holds and not
    /// only on a later change.
    async fn spawn_cancellable(
        self: &Arc<Self>,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let mut cancelled = self.cancel.subscribe();
        self.workers.lock().await.spawn(async move {
            tokio::select! {
                // The cancel arm first, so a worker that starts after the
                // lifetime ended does no work before it sees that.
                biased;
                _ = cancelled.wait_for(|stopped| *stopped) => {}
                // Dropped when the arm above wins, which is what hands the
                // admission permit back: it is owned by this future.
                () = work => {}
            }
        });
    }

    /// Select a borrowed, oldest-first prefix without collecting or scanning the backlog.
    /// The fixed limit bounds scheduling even when failed workers return permits during the loop.
    pub(crate) fn repair_batch<'a, I>(
        &self,
        unrepaired: I,
    ) -> impl Iterator<Item = &'a UnconfirmedSettlement> + use<'a, I, T>
    where
        I: IntoIterator<Item = &'a UnconfirmedSettlement>,
    {
        unrepaired.into_iter().take(self.limits.max_in_flight)
    }

    /// Take the claim on one repair identity, or answer that another attempt
    /// already holds it.
    fn claim_repair(&self, session_id: &SessionId, call_id: &ResponseId) -> Option<RepairClaim> {
        let identity = (session_id.clone(), call_id.clone());
        self.repair_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(identity.clone())
            .then(|| RepairClaim {
                held: Arc::clone(&self.repair_claims),
                identity,
            })
    }

    /// The repair worker body. Holds no session lease and opens no writer.
    async fn run_repair(
        self: Arc<Self>,
        capacity: Capacity,
        claim: RepairClaim,
        session_id: SessionId,
        principal: Principal,
        settlement: UnconfirmedSettlement,
    ) {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(self.limits.call_ttl_ms);
        let Some(applied) = self
            .shadow
            .repair_settlement(&principal, &session_id, &settlement, deadline)
            .await
        else {
            // No answer. The settlement stays unrepaired in the log, so a later
            // turn drives it again — which is why nothing is parked here, and
            // why both the permit and the claim are simply released. Releasing
            // the claim on a failure is what keeps "one attempt at a time" from
            // becoming "one attempt ever".
            return;
        };
        // Both carried onto the acknowledgement rather than released here: it
        // is undelivered work against this process until a turn commits it, so
        // it occupies admission, and the settlement it answers still reads as
        // unrepaired in the log until then, so its identity stays claimed.
        let now_ms = roundhouse_core::now_ms();
        let record = ClassificationSettlementRepair {
            call_id: settlement.call_id,
            applied,
            repaired_at_ms: now_ms,
        };
        let retain_until_ms = now_ms.saturating_add(self.limits.result_retention_ms);
        self.repairs
            .park(session_id, record, retain_until_ms, capacity.0, Some(claim))
            .await;
    }

    /// Repair acknowledgements this session has waiting, without giving them up.
    pub async fn ready_repairs(
        &self,
        session_id: &SessionId,
    ) -> Vec<Delivered<ClassificationSettlementRepair>> {
        let waiting = self.repairs.ready(session_id).await;
        #[cfg(any(test, feature = "test-support"))]
        self.repair_handles_issued
            .fetch_add(waiting.len(), Ordering::Relaxed);
        waiting
    }

    /// How many acknowledgements this session is holding.
    ///
    /// Reads the length rather than taking handles, so a test can watch
    /// retention without moving [`Self::repair_handles_issued`] by looking.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn retained_repairs(&self, session_id: &SessionId) -> usize {
        self.repairs.retained(session_id).await
    }

    /// Retained acknowledgements handed to a turn since this runtime started.
    #[cfg(any(test, feature = "test-support"))]
    pub fn repair_handles_issued(&self) -> usize {
        self.repair_handles_issued.load(Ordering::Relaxed)
    }

    /// Repair identities some attempt is holding right now.
    ///
    /// The only way to observe a claim a cancelled worker was supposed to
    /// release: cancellation leaves no record of its own, and a stopped runtime
    /// refuses a second attempt for a reason that has nothing to do with the
    /// claim, so behaviour alone cannot tell a released claim from a leaked one
    /// there.
    #[cfg(test)]
    fn claimed_repairs(&self) -> usize {
        self.repair_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Drop the repair acknowledgements a writer committed, releasing the
    /// permit and the identity claim each was holding.
    pub async fn acknowledge_repairs(&self, session_id: &SessionId, written: &[ResponseId]) {
        self.repairs.acknowledge(session_id, written).await;
    }

    /// Record that a call expired before anything was sent.
    ///
    /// **Durable, and stronger than silence.** No grant was opened and no socket
    /// was touched, so this deployment knows the call cost nothing — which is a
    /// different statement from the one an intent with no result at all makes,
    /// and that one still means "a process died and nobody can say".
    ///
    /// **Only reachable before the grant**, which is what lets it say that much:
    /// both callers are in [`Self::run`], ahead of
    /// [`TypeSafeShadow::execute`](crate::typesafe_shadow::TypeSafeShadow::execute).
    /// A call whose life runs out *inside* the grant records the same
    /// [`FundingRefusal::Expired`] from there instead, and that one claims less:
    /// the ledger may have opened a hold this worker stopped waiting to hear
    /// about.
    async fn park_unfunded(
        &self,
        session_id: SessionId,
        call: PreparedCall,
        reason: FundingRefusal,
        capacity: Capacity,
    ) {
        let record = ClassificationRecord {
            call_id: call.intent.call_id.clone(),
            source_turn_index: call.intent.source_turn_index,
            source_response_id: call.intent.source_response_id.clone(),
            completed_at_ms: roundhouse_core::now_ms(),
            outcome: ClassificationOutcome::Unfunded { reason },
        };
        self.park(session_id, record, capacity).await;
    }

    /// Hold a finished result until a turn with a writer takes it.
    async fn park(&self, session_id: SessionId, record: ClassificationRecord, capacity: Capacity) {
        let retain_until_ms =
            roundhouse_core::now_ms().saturating_add(self.limits.result_retention_ms);
        self.results
            .park(session_id, record, retain_until_ms, capacity.0, None)
            .await;
    }

    /// What this session has waiting, without giving it up.
    pub async fn ready(&self, session_id: &SessionId) -> Vec<Delivered<ClassificationRecord>> {
        self.results.ready(session_id).await
    }

    /// Drop what a writer committed.
    pub async fn acknowledge(&self, session_id: &SessionId, delivered: &[ResponseId]) {
        self.results.acknowledge(session_id, delivered).await;
    }

    /// Reclaim results and repair acknowledgements nobody came back for, and
    /// join finished workers.
    ///
    /// Returns how many *results* it dropped, so a test can assert that an
    /// idle session is reclaimed without reaching into the map. A dropped
    /// repair acknowledgement is not counted in that: losing one costs a
    /// redundant deduplicated ledger call on a later turn and nothing else,
    /// while losing a result loses an answer that was paid for — the two are
    /// not the same kind of loss, so they are not the same number.
    pub async fn sweep(&self, now_ms: u64) -> usize {
        let dropped = self.results.sweep(now_ms).await;
        self.repairs.sweep(now_ms).await;
        // A bounded result map over an unbounded `JoinSet` is not bounded.
        let mut workers = self.workers.lock().await;
        while workers.try_join_next().is_some() {}
        dropped
    }

    /// Stop accepting work, abort what is running, and release everything held.
    ///
    /// **The explicit form of [`Self::stop`], and it waits.** Stopping is the
    /// part `Drop` can do; this additionally joins every worker and drops the
    /// results nobody drained, so a caller that awaits it knows the runtime is
    /// quiet rather than merely told to be.
    ///
    /// An aborted call's hold is not settled here and lapses on its TTL: the
    /// worker was cancelled mid-flight, so whether the service billed is exactly
    /// what this process does not know.
    ///
    /// **Test-only.** No production path calls it: the production lifetime is
    /// [`Supervisor`]'s `Drop`, which calls [`Self::stop`] and returns
    /// immediately rather than waiting for it. This exists so a recovery test
    /// can simulate a clean process restart deterministically — waiting for
    /// the runtime to go quiet rather than racing its own teardown.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn shutdown(&self) {
        self.stop();
        self.workers.lock().await.shutdown().await;
        self.results.clear().await;
        // Safe to drop for the reason the sweep may drop them: the log still
        // says these settlements are unrepaired, so the next process re-drives
        // them and the ledger deduplicates.
        self.repairs.clear().await;
    }

    /// Start the supervisor that sweeps and reaps on its own clock, and take
    /// ownership of this runtime's lifetime.
    ///
    /// **A task rather than a hook on the turn path**, because the sessions
    /// whose results strand are precisely the ones with no next turn to run a
    /// hook.
    pub fn supervise(self: &Arc<Self>) -> Supervisor<T> {
        let sweeper = Arc::clone(self);
        let every = std::time::Duration::from_millis(self.limits.sweep_interval_ms);
        Supervisor {
            sweep: tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    sweeper.sweep(roundhouse_core::now_ms()).await;
                }
            }),
            runtime: Arc::clone(self),
        }
    }
}

/// Build the runtime a configuration describes, or answer that there is none.
///
/// **In the library, not in `main`.** `shared_backend`'s own module doc records
/// what wiring inside a `[[bin]]` cost the last time: nothing outside the binary
/// could call it, so a mutation of the wiring left the whole workspace green.
/// The composition root calls this and wires what it returns.
///
/// `None` for a deployment whose file exists and says `enabled: false`, which is
/// the same answer as no file at all — deliberately, so a deployment that wrote
/// the configuration and has not turned it on runs the code path of one that
/// never wrote it. An `Err` is a file that *is* enabled and cannot be honoured;
/// that stops the process, the same posture the catalog and the control plane
/// take.
pub fn compose<T: Tokenizer + Send + Sync + 'static>(
    path: &str,
    config: &ClassifyConfig,
    evaluation_spend: Arc<dyn SpendLedger>,
    tokenizer: T,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<Arc<ClassificationRuntime<T>>>> {
    if !config.enabled {
        return Ok(None);
    }
    // The key before anything else: an enabled classifier with no credential is
    // a boot refusal, not a runtime surprise on the first turn that would have
    // used it.
    let credential = config.credential(path, env)?;
    let client = SystemOneClient::new(config.base_url.trim(), config.transport_limits())
        .map_err(|error| anyhow::anyhow!("building the classification transport: {error}"))?;
    let shadow = TypeSafeShadow::new(client, config.shadow_config(), evaluation_spend, tokenizer);
    Ok(Some(Arc::new(ClassificationRuntime::new(
        shadow,
        config.budget_terms(),
        credential,
        config.runtime_limits(),
    ))))
}

/// One deployment's classification lifetime, held for as long as it serves.
///
/// **Ending it ends the runtime**, which is why this owns more than a task
/// handle. It used to abort the sweep and nothing else, so the lifetime a
/// deployment actually got left admission open and a worker on the wire running
/// against an upstream nobody was waiting for any more;
/// [`ClassificationRuntime::shutdown`] stopped both and nothing in production
/// called it. The composition root holds this and never calls anything on it —
/// the guarantee is in the drop, so it cannot be forgotten at one of the
/// several places serving can end.
pub struct Supervisor<T: Tokenizer> {
    sweep: tokio::task::JoinHandle<()>,
    runtime: Arc<ClassificationRuntime<T>>,
}

impl<T: Tokenizer> Drop for Supervisor<T> {
    fn drop(&mut self) {
        self.sweep.abort();
        self.runtime.stop();
    }
}

#[cfg(test)]
mod tests;
