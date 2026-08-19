// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The session state machine.
//!
//! A [`Session`] is a lease plus a projection. Everything it knows — the
//! conversation, the routing ledger, which turns are already done — is derived
//! by replaying the event log, so a successor process that acquires the lease
//! reconstructs identical state without any handoff from the process it
//! replaced.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::control::{FrontierHistory, Principal};
use crate::event::{IncompleteReason, SessionEvent, SessionEventKind, SessionObserver, Usage};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::item::{Item, ItemContent};
use crate::routing::{CacheLedger, DecisionRecord, ProviderPricing, Target};
use crate::store::{Lease, SessionStore, StoreError};

/// How many events to pull per replay batch.
const REPLAY_BATCH: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("session `{0}` is owned by another node")]
    NotOwner(SessionId),
    #[error("response `{0}` is not open")]
    ResponseNotOpen(ResponseId),
}

/// The result of admitting a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnAdmission {
    /// A new turn was opened.
    Started(ResponseId),
    /// The turn id was already completed; the existing response is replayed
    /// rather than a second one being generated.
    Deduplicated(ResponseId),
}

impl TurnAdmission {
    pub fn response_id(&self) -> &ResponseId {
        match self {
            TurnAdmission::Started(id) | TurnAdmission::Deduplicated(id) => id,
        }
    }
}

/// What a completed turn produced.
///
/// The usage is kept, not just the response id, because a deduplicated retry
/// has to answer with the accounting the original turn reported. Recomputing it
/// is not an option — the token counts came from the provider — and reporting
/// zeros would tell a client its turn was free.
struct CompletedTurn {
    response_id: ResponseId,
    usage: Usage,
}

/// Where a dispatch went, how much prompt it carried, and at what price.
struct PendingRouting {
    target: Target,
    isl_tokens: u64,
    /// The rate card the decision recorded. See
    /// [`DecisionRecord::rate_card`] for why a settle reads the log's card and
    /// never a live one.
    rate_card: Option<ProviderPricing>,
}

/// A response that terminated, and everything needed to charge it for.
///
/// The spend ledger's view of a turn, projected from the log rather than
/// handed across from whatever produced it: the repair that re-drives a lost
/// settle has only the log to work from, so if the live settle read its inputs
/// from anywhere else the two would be settling from two different accounts of
/// the same turn.
///
/// **The price is one of those inputs**, which it was not at first. A settle
/// priced against the running process's catalog is priced against a file an
/// operator edits, so the two moments really could see different numbers — and
/// a repair against a catalog that had *dropped* the model could see no number
/// at all, which failed the settle, which failed every turn of the session
/// after it. [`Self::rate_card`] is that hole closed at the seam it was open
/// at: the card travels in the log beside the target it prices.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSettlement {
    pub response_id: ResponseId,
    /// The log sequence number of the terminal event, and half of the
    /// idempotency key a settle is applied under.
    pub seq: u64,
    /// Where the turn was dispatched.
    ///
    /// `None` for a response that terminated before any routing decision was
    /// recorded — a refusal, or a failure while pricing the options. That turn
    /// reached no provider and so owes nobody anything, which makes the
    /// absence a fact rather than a missing value.
    pub target: Option<Target>,
    /// The rate card that was in force when the turn was routed, as its
    /// decision recorded it — the price half of "projected from the log rather
    /// than handed across from whatever produced it".
    ///
    /// Carried beside the target rather than folded into it, because the two
    /// absences mean different things and a settle has to tell them apart: no
    /// target is a turn that reached nobody and owes nothing, while a frontier
    /// target with no card is a turn routed before the card was recorded, which
    /// no process alive can price. See [`DecisionRecord::rate_card`].
    pub rate_card: Option<ProviderPricing>,
    /// What the terminal event reported, estimate or measurement alike. An
    /// estimate is what a provider that reported nothing gets charged on, and
    /// it is charged exactly as a measurement would be.
    pub usage: Usage,
}

/// State derived from the event log.
#[derive(Default)]
pub struct SessionState {
    pub items: Vec<Item>,
    pub ledger: CacheLedger,
    /// Number of turns started so far; the index the next turn will use.
    pub turn_index: u64,
    /// Turn ids that reached a terminal state, and what they produced.
    completed_turns: HashMap<TurnId, CompletedTurn>,
    /// Turn ids currently in flight.
    open_turns: HashMap<TurnId, ResponseId>,
    /// Routing facts of responses that have not terminated yet.
    ///
    /// `Routed` is committed before execution, so it records an intent rather
    /// than a transmission and is the wrong evidence for the cache ledger: a
    /// dispatch that failed on the way out would leave a phantom warm prefix
    /// on that target, and the retry would then be priced against a cache that
    /// does not exist — phantom-warm frontier beating genuinely cheaper local
    /// workers. The fold therefore happens at the terminal event, and only
    /// when that event proves the prompt was processed: a completion always
    /// does, an incomplete does only when its usage shows billed input
    /// (partial output cannot exist without a prefill, but the engine also
    /// terminates dispatches that failed before anything was sent, and those
    /// carry an empty usage). A response that never terminates — the process
    /// died mid-flight, and nobody knows what the provider saw — records
    /// nothing. Cold is the conservative reading in both unproven cases,
    /// because over-claiming warmth is the exact mispricing this fold exists
    /// to prevent.
    pending_routings: HashMap<ResponseId, PendingRouting>,
    /// Which routed turns went to a hosted model, in log order.
    ///
    /// Folded at `Routed` and *not* at the terminal event, which is the
    /// opposite of the rule `pending_routings` follows one field up, so the
    /// difference is worth stating. The cache ledger asks "did the provider
    /// process this prompt?", and a dispatch that failed on the way out is no
    /// evidence of that. A cadence asks "how often did this session reach for
    /// a hosted model?", and a dispatch that failed on the way out is exactly
    /// that. Counting only successes would let a provider outage multiply the
    /// frontier traffic of every session retrying through it, at the moment
    /// the knob is most supposed to hold.
    pub frontier_history: FrontierHistory,
    /// The most recent response to terminate, priced-ready.
    ///
    /// **One entry rather than a list, and that is a claim about the log
    /// rather than a convenience.** A session's turns are serialized — the
    /// engine gates them per session within a process and the store's lease
    /// fences them across processes — and every turn applies its spend before
    /// the next is admitted. So whenever a session is opened, at most one
    /// terminal event can still be unsettled, and it is the last one. Keeping
    /// the whole history here would be an unbounded second copy of the log,
    /// held to support a repair that can only ever concern its final entry.
    ///
    /// `None` until a response terminates, which is also the honest answer for
    /// a session whose only turns are still open.
    last_settlement: Option<TerminalSettlement>,
    /// Steers this deployment emitted that no client has answered yet, keyed
    /// by `call_id` and valued by the log timestamp of the `ItemAppended` that
    /// opened each one.
    ///
    /// The timestamp is what makes fulfilment latency derivable from the
    /// projection and the log alone — the closing item's own `at_ms` minus
    /// this one — without a second table to keep in step with the first.
    ///
    /// Filled by an `ItemAppended` whose item is a `ToolCall` *bearing a
    /// response id*, and cleared by an `ItemAppended` whose item is a
    /// `ToolResult` naming the same call. Provenance rather than shape is what
    /// selects the opening item: everything on the input path canonicalizes
    /// with no response id, so a client cannot open a steer by sending a tool
    /// call. One writer, one event kind, no second source of truth.
    ///
    /// `pub(crate)` rather than `pub` because its consumer is M6's trigger —
    /// the open-steer exclusion that stops a steer re-triggering the
    /// validation that emitted it. It ships with M4 anyway because it is a
    /// fact about M4's items: the events that fill it are emitted here, and a
    /// projection added later against an older log is a projection nobody has
    /// replayed.
    pub(crate) open_steers: HashMap<String, u64>,
    /// The most recent routing decision, whether or not its response has
    /// terminated.
    ///
    /// A different question from [`Self::pending_routings`] one field up, which
    /// is why it is a different field rather than a read of that one.
    /// `pending_routings` asks *what evidence is still outstanding* and is
    /// emptied at the terminal event; this asks *where did the last turn go*,
    /// which is exactly as true after the turn ends as during it. Deriving one
    /// from the other would answer "this session has never been routed" for
    /// every session between turns.
    ///
    /// The whole record rather than the target: its consumer renders the
    /// rationale, the policy digest and the budget state as well — see
    /// `explain_last_route` in `roundhouse-mcp`, the audit trail as a tool.
    last_decision: Option<DecisionRecord>,
    pub last_seq: u64,
}

impl SessionState {
    /// Fold one event into the projection.
    ///
    /// The exhaustive match is deliberate: a new event kind should not silently
    /// fail to update derived state.
    fn apply(&mut self, event: &SessionEvent) {
        self.last_seq = event.seq;
        match &event.kind {
            SessionEventKind::ItemAppended { item } => {
                // The conversation itself is untouched by steering: an emitted
                // tool call is an ordinary item arriving on the ordinary path,
                // which is the whole reason this kind was reused rather than a
                // second item-carrying kind added. A second kind would have
                // given this fold, the wire layer's stored-items projection and
                // the context assembler two sources for one conversation, and
                // the first site to forget the second kind forks every steered
                // session.
                self.items.push(item.clone());
                // The projection beside it, folded from the same event. See
                // `open_steers`.
                match &item.content {
                    ItemContent::ToolCall { call_id, .. } => {
                        // Provenance, not shape. An item on the input path
                        // canonicalizes with no response id, so this arm is
                        // unreachable for anything a client sent — including
                        // the client's own verbatim resend of the very call it
                        // is about to answer, which must not re-open the steer
                        // its output closes.
                        if item.response_id.is_some() {
                            self.open_steers.insert(call_id.clone(), event.at_ms);
                        }
                    }
                    ItemContent::ToolResult { call_id, .. } => {
                        // Keyed on the call id and not on "a result arrived":
                        // an agent runs its own tools between our turns, and
                        // closing on any of those would report a steer
                        // fulfilled that nobody answered.
                        self.open_steers.remove(call_id);
                    }
                    ItemContent::Text { .. } => {}
                }
            }
            SessionEventKind::TurnStarted {
                turn_id,
                response_id,
            } => {
                self.turn_index += 1;
                self.open_turns.insert(turn_id.clone(), response_id.clone());
            }
            SessionEventKind::Routed {
                response_id,
                decision,
            } => {
                self.frontier_history.record(&decision.chosen);
                self.last_decision = Some(decision.clone());
                // Held rather than recorded; see `pending_routings`.
                self.pending_routings.insert(
                    response_id.clone(),
                    PendingRouting {
                        target: decision.chosen.clone(),
                        isl_tokens: decision.isl_tokens,
                        rate_card: decision.rate_card,
                    },
                );
            }
            SessionEventKind::ResponseCompleted { response_id, usage }
            | SessionEventKind::ResponseIncomplete {
                response_id, usage, ..
            } => {
                // Recorded at the terminal event's timestamp — when the
                // provider stopped holding the prompt, which is what the TTL
                // runs from — and only under the evidence rule documented on
                // `pending_routings`.
                let routing = self.pending_routings.remove(response_id);
                if let Some(routing) = &routing {
                    let processed =
                        matches!(event.kind, SessionEventKind::ResponseCompleted { .. })
                            || usage.input_tokens > 0;
                    if processed {
                        self.ledger
                            .record(&routing.target, event.at_ms, routing.isl_tokens);
                    }
                }

                // The spend ledger's view of the same event, and note that it
                // is folded under *no* evidence rule: the cache ledger asks
                // whether a provider processed the prompt, while a settlement
                // asks what the log says this turn is to be charged. A
                // dispatch that never reached anyone is priced at zero by
                // carrying no target, not by being left out — leaving it out
                // would strand its hold for a whole TTL.
                self.last_settlement = Some(TerminalSettlement {
                    response_id: response_id.clone(),
                    seq: event.seq,
                    rate_card: routing.as_ref().and_then(|routing| routing.rate_card),
                    target: routing.map(|routing| routing.target),
                    usage: usage.clone(),
                });

                // A turn is only settled once its response terminates, which is
                // what makes a re-sent turn after a mid-generation crash
                // restartable rather than deduplicated into a partial answer.
                if let Some(turn_id) = self
                    .open_turns
                    .iter()
                    .find(|(_, open)| *open == response_id)
                    .map(|(turn_id, _)| turn_id.clone())
                {
                    self.open_turns.remove(&turn_id);
                    if matches!(event.kind, SessionEventKind::ResponseCompleted { .. }) {
                        self.completed_turns.insert(
                            turn_id,
                            CompletedTurn {
                                response_id: response_id.clone(),
                                usage: usage.clone(),
                            },
                        );
                    }
                }
            }
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::OutputTextDelta { .. }
            | SessionEventKind::TurnDeduplicated { .. }
            | SessionEventKind::Error { .. } => {}
        }
    }

    pub fn completed_response_for(&self, turn_id: &TurnId) -> Option<&ResponseId> {
        self.completed_turns
            .get(turn_id)
            .map(|completed| &completed.response_id)
    }

    /// The most recent response to terminate, and what it is to be charged
    /// for.
    ///
    /// The one input the spend ledger's settle takes, both when this turn
    /// commits its own terminal event and when a successor replays a log whose
    /// last settle was lost to a crash. See [`Self::last_settlement`] on why
    /// one entry is enough for both.
    pub fn last_settlement(&self) -> Option<&TerminalSettlement> {
        self.last_settlement.as_ref()
    }

    /// Usage reported when `turn_id` completed, for replaying it verbatim.
    pub fn completed_usage_for(&self, turn_id: &TurnId) -> Option<&Usage> {
        self.completed_turns
            .get(turn_id)
            .map(|completed| &completed.usage)
    }

    /// Where the most recent routed turn went, and why.
    ///
    /// `None` for a session whose first turn has not been routed — a session
    /// that has only ever been steered has no routing decision, which is the
    /// accurate answer rather than an empty one.
    pub fn last_decision(&self) -> Option<&DecisionRecord> {
        self.last_decision.as_ref()
    }

    /// `call_id`s of steers this deployment emitted that no turn has answered.
    ///
    /// Sorted, so two reads of one projection are two identical answers: the
    /// underlying map has no order, and this is rendered into an agent's
    /// context by `status`, where a list that reshuffled between calls is a
    /// list an agent reads as having changed.
    ///
    /// The accessor rather than the field: [`Self::open_steers`] stays private
    /// because it is a fold this module owns, and its two consumers — M5's
    /// `status` tool and M6's trigger — both want the ids and neither wants the
    /// timestamps.
    pub fn open_steer_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.open_steers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Rebuild a session's projection from its log, **taking no lease**.
    ///
    /// The read-only half of [`Session::open_observed`], which calls it: a
    /// reader that took the lease would evict the engine it is watching, and
    /// that rule is why the MCP control surface projects a session through here
    /// rather than opening one. One replay loop and one fold serve both, so a
    /// question answered for a reader and the same question answered for the
    /// engine cannot come back with two answers.
    pub async fn project<S: SessionStore>(
        store: &S,
        session_id: &SessionId,
        ledger: CacheLedger,
        observer: Option<&Arc<dyn SessionObserver>>,
    ) -> Result<Self, SessionError> {
        let mut state = SessionState {
            ledger,
            ..Default::default()
        };
        // Replay in batches so a long session does not need the whole log
        // resident at once.
        let mut cursor = 0u64;
        loop {
            let batch = store.read_events(session_id, cursor, REPLAY_BATCH).await?;
            if batch.is_empty() {
                break;
            }
            for event in &batch {
                state.apply(event);
            }
            if let Some(observer) = observer {
                observer.observe(&batch);
            }
            cursor = batch.last().map_or(cursor, |event| event.seq);
        }
        Ok(state)
    }
}

/// A running lease renewal, alive for exactly as long as this handle is.
///
/// Dropping it stops the renewal at that instant rather than at the next tick,
/// so the lease a finished turn leaves behind lapses on the normal failover
/// clock instead of being carried by a task nobody is watching any more.
pub struct LeaseHeartbeat {
    task: tokio::task::AbortHandle,
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A leased, projected session.
pub struct Session<S: SessionStore> {
    store: Arc<S>,
    session_id: SessionId,
    lease: Lease,
    state: SessionState,
    /// Watches every event this session commits, and every event it replayed
    /// on open. See [`Session::open_observed`].
    observer: Option<Arc<dyn SessionObserver>>,
}

impl<S: SessionStore> Session<S> {
    /// Claim a session and rebuild its state from the log.
    ///
    /// `ledger` carries the cache-model and pricing configuration; replayed
    /// dispatches that reached a terminal event are folded into it, so what
    /// comes back reflects both static configuration and session history.
    pub async fn open(
        store: Arc<S>,
        session_id: SessionId,
        node_id: &str,
        lease_ttl_ms: u64,
        ledger: CacheLedger,
    ) -> Result<Self, SessionError> {
        Self::open_observed(store, session_id, node_id, lease_ttl_ms, ledger, None).await
    }

    /// Claim a session with something watching its log.
    ///
    /// The observer sees the replay as well as subsequent commits, which is
    /// what lets a restarted process recover derived state for the sessions it
    /// picks back up rather than only for the turns it goes on to serve. That
    /// is only sound because the metrics fold is idempotent by `(session,
    /// seq)`; an observer without that property would double-count every
    /// session that is opened twice, which is every session that takes more
    /// than one turn.
    pub async fn open_observed(
        store: Arc<S>,
        session_id: SessionId,
        node_id: &str,
        lease_ttl_ms: u64,
        ledger: CacheLedger,
        observer: Option<Arc<dyn SessionObserver>>,
    ) -> Result<Self, SessionError> {
        let lease = store
            .acquire_lease(&session_id, node_id, lease_ttl_ms)
            .await?
            .ok_or_else(|| SessionError::NotOwner(session_id.clone()))?;

        // The same replay a lease-free reader performs, so the two cannot
        // diverge; the lease above is the only thing this adds to it.
        let state =
            SessionState::project(store.as_ref(), &session_id, ledger, observer.as_ref()).await?;

        Ok(Self {
            store,
            session_id,
            lease,
            state,
            observer,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn ledger(&self) -> &CacheLedger {
        &self.state.ledger
    }

    /// Turn index the next admitted turn will receive.
    pub fn turn_index(&self) -> u64 {
        self.state.turn_index
    }

    pub fn last_seq(&self) -> u64 {
        self.state.last_seq
    }

    /// Extend the lease. `false` means ownership was lost and this session
    /// handle must be discarded.
    pub async fn renew(&mut self, ttl_ms: u64) -> Result<bool, SessionError> {
        match self.store.renew_lease(&self.lease, ttl_ms).await? {
            Some(renewed) => {
                self.lease = renewed;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Renew this session's lease every `every_ms` until the handle is dropped.
    ///
    /// A model call routinely outlasts any TTL worth failing over on, and the
    /// TTL cannot simply be raised to cover it: it *is* the failover clock, the
    /// time a successor must wait before it may assume this node is gone.
    /// Renewal separates the two, so the lease lives as long as the owner is
    /// demonstrably alive while a genuinely dead owner is still replaced one
    /// TTL after its last tick.
    ///
    /// Renewal extends a lease that is still valid and does nothing else. The
    /// moment the store says otherwise — `None`, or an error that leaves the
    /// renewal unproven — the task stops for good. Re-acquiring here is
    /// the tempting repair and it is exactly the split-brain bug: a successor
    /// has already been admitted on the strength of this lease lapsing, and a
    /// second writer appending into the same log is the one thing leases exist
    /// to prevent. Stopping instead leaves fencing to fail this owner's next
    /// append, which is the correct outcome and the one the store guarantees.
    ///
    /// Renewal is only safe because the work it covers is bounded; an owner
    /// that is alive but stuck must still lose the session. The engine's
    /// `turn_deadline_ms` is what supplies that bound.
    pub fn heartbeat(&self, every_ms: u64, ttl_ms: u64) -> LeaseHeartbeat {
        let store = Arc::clone(&self.store);
        // The task owns a clone, so it renews without the session handle. Both
        // stay usable: [`SessionStore::append_events`] validates against the
        // store's current record rather than against the handle's copy of
        // `expires_at_ms`.
        let mut lease = self.lease.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(every_ms)).await;
                match store.renew_lease(&lease, ttl_ms).await {
                    Ok(Some(renewed)) => lease = renewed,
                    Ok(None) => {
                        tracing::warn!(
                            session_id = %lease.session_id,
                            node_id = %lease.node_id,
                            "lease lost; stopping renewal rather than re-acquiring"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            session_id = %lease.session_id,
                            node_id = %lease.node_id,
                            %error,
                            "lease renewal failed; stopping renewal"
                        );
                        return;
                    }
                }
            }
        });
        LeaseHeartbeat {
            task: task.abort_handle(),
        }
    }

    pub async fn release(self) -> Result<(), SessionError> {
        Ok(self.store.release_lease(&self.lease).await?)
    }

    /// Append events and fold them into the local projection.
    ///
    /// The projection is updated from what the store actually assigned rather
    /// than from what was submitted, so in-memory state can never claim a
    /// sequence number the log does not have.
    async fn commit(
        &mut self,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let events = self.store.append_events(&self.lease, kinds).await?;
        for event in &events {
            self.state.apply(event);
        }
        // After the projection, not before: an observer that read session state
        // in response would otherwise see it one commit behind the events it
        // was just handed.
        if let Some(observer) = &self.observer {
            observer.observe(&events);
        }
        Ok(events)
    }

    /// Record the session-created event. Safe to call once, at creation.
    ///
    /// Called where the log is still empty and this handle already holds the
    /// lease, which is what makes "exactly once" a property of the log rather
    /// than of anyone's care: a second caller would have to win the lease and
    /// find `last_seq == 0`, and only one of those can be true at a time.
    ///
    /// The principal is taken by reference and not as an `Option`, so a
    /// `SessionCreated` this method writes always names its payer. That is what
    /// makes the absent case in the event unambiguous — it can only mean a log
    /// older than tenancy, never "this deployment forgot".
    pub async fn record_created(
        &mut self,
        model_policy: &str,
        principal: &Principal,
    ) -> Result<(), SessionError> {
        self.commit(vec![SessionEventKind::SessionCreated {
            model_policy: model_policy.to_string(),
            principal: Some(principal.clone()),
        }])
        .await?;
        Ok(())
    }

    /// Admit a turn, appending its input items.
    ///
    /// A `turn_id` that already completed short-circuits: the prior response is
    /// returned and no items are appended, which is what makes a client retry
    /// after a dropped connection safe.
    pub async fn begin_turn(
        &mut self,
        turn_id: TurnId,
        input: Vec<Item>,
    ) -> Result<TurnAdmission, SessionError> {
        if let Some(existing) = self.state.completed_response_for(&turn_id).cloned() {
            self.commit(vec![SessionEventKind::TurnDeduplicated {
                turn_id,
                response_id: existing.clone(),
            }])
            .await?;
            return Ok(TurnAdmission::Deduplicated(existing));
        }

        let response_id = ResponseId::generate();
        let mut kinds = Vec::with_capacity(input.len() + 1);
        kinds.push(SessionEventKind::TurnStarted {
            turn_id,
            response_id: response_id.clone(),
        });
        kinds.extend(
            input
                .into_iter()
                .map(|item| SessionEventKind::ItemAppended { item }),
        );
        self.commit(kinds).await?;
        Ok(TurnAdmission::Started(response_id))
    }

    /// Record the routing choice before any execution begins.
    ///
    /// This is the audit trail, not yet ledger evidence: the dispatch is only
    /// folded into the cache ledger once its response terminates. See
    /// [`SessionState`]'s pending routings.
    pub async fn record_routing(
        &mut self,
        response_id: &ResponseId,
        decision: DecisionRecord,
    ) -> Result<(), SessionError> {
        self.commit(vec![SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision,
        }])
        .await?;
        Ok(())
    }

    pub async fn append_output(
        &mut self,
        response_id: &ResponseId,
        text: impl Into<String>,
    ) -> Result<SessionEvent, SessionError> {
        let mut events = self
            .commit(vec![SessionEventKind::OutputTextDelta {
                response_id: response_id.clone(),
                text: text.into(),
            }])
            .await?;
        events
            .pop()
            .ok_or_else(|| SessionError::ResponseNotOpen(response_id.clone()))
    }

    /// Close a response successfully, committing the assistant item.
    ///
    /// The dispatched turn's spelling of [`Session::complete_with_item`], and
    /// expressed in terms of it rather than beside it: the atomicity rule — the
    /// produced item and the terminal event in one append batch — is a property
    /// of *completing*, not of completing with a particular shape of item, and
    /// stating it twice is how the two spellings drift apart.
    pub async fn complete(
        &mut self,
        response_id: &ResponseId,
        text: impl Into<String>,
        usage: Usage,
    ) -> Result<(), SessionError> {
        self.complete_with_item(
            response_id,
            Item::assistant_text(text, response_id.clone()),
            usage,
        )
        .await
    }

    /// Close a response successfully, committing `item` as what it produced.
    ///
    /// The one way a response completes. [`Session::complete`] is this method
    /// with the item fixed to assistant text, which is the shape every
    /// dispatched turn produces; a steered turn passes its synthetic tool call
    /// instead. Both spellings commit the same two events in the same batch
    /// because they are the same method, rather than because two methods agree.
    ///
    /// **The response id is stamped here rather than read off the item.** That
    /// is what makes a committed item bearing a response id mean "this response
    /// emitted it": everything on the input path arrives through
    /// [`Session::begin_turn`] carrying none, and this is the only method that
    /// puts one on anything at all. A caller that had to supply the stamp
    /// itself could forget it, and a forgotten stamp is an emitted call that no
    /// projection can tell from a client's own.
    ///
    /// **One append batch, never two.** The item and the completion are a
    /// decision and its realization. Committed separately, a process that died
    /// between them would leave a session holding an emitted tool call whose
    /// turn never completed: the retry would not deduplicate, the same call
    /// would be emitted a second time, and the client would hold two calls with
    /// one id. The store numbers a batch contiguously within one call, so the
    /// window does not exist rather than being narrow — the same property
    /// [`Session::begin_turn`] admits a turn and its input under.
    ///
    /// **Nothing here records a [`SessionEventKind::Routed`]**, and for the
    /// steered caller that absence is the point rather than an omission: the
    /// usage passed is whatever produced the interjection, and the turn itself
    /// dispatched nothing. A turn that reaches its completion with no routing
    /// of its own is priced at nothing by every projection that pairs a
    /// dispatch with its terminal event — the metrics fold books no model row
    /// for it and the cache ledger records no warm prefix — while the client is
    /// still told what its turn genuinely cost. (A dispatched turn recorded its
    /// own `Routed` before it got here, so the same absence costs it nothing.)
    /// See
    /// [`interject`](crate::interject) for why the decision is taken before the
    /// turn is planned, which is what makes that pairing absent rather than
    /// merely unused.
    pub async fn complete_with_item(
        &mut self,
        response_id: &ResponseId,
        item: Item,
        usage: Usage,
    ) -> Result<(), SessionError> {
        let item = Item {
            response_id: Some(response_id.clone()),
            ..item
        };
        self.commit(vec![
            SessionEventKind::ItemAppended { item },
            SessionEventKind::ResponseCompleted {
                response_id: response_id.clone(),
                usage,
            },
        ])
        .await?;
        Ok(())
    }

    /// Close a response without a complete answer.
    ///
    /// The partial text is committed as an assistant item so the successor can
    /// resume from it. That partial is also, conveniently, a guaranteed cache
    /// hit on the target that produced it.
    pub async fn mark_incomplete(
        &mut self,
        response_id: &ResponseId,
        partial: impl Into<String>,
        reason: IncompleteReason,
        usage: Usage,
    ) -> Result<(), SessionError> {
        let partial = partial.into();
        let mut kinds = Vec::with_capacity(2);
        if !partial.is_empty() {
            kinds.push(SessionEventKind::ItemAppended {
                item: Item::assistant_text(partial, response_id.clone()),
            });
        }
        kinds.push(SessionEventKind::ResponseIncomplete {
            response_id: response_id.clone(),
            reason,
            usage,
        });
        self.commit(kinds).await?;
        Ok(())
    }

    /// Events after `after_seq`. Backs both SSE resumption and reconnect replay.
    pub async fn events_since(
        &self,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        Ok(self
            .store
            .read_events(&self.session_id, after_seq, limit)
            .await?)
    }

    /// Events belonging to one response, for the Responses API view.
    pub async fn response_events(
        &self,
        response_id: &ResponseId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        Ok(self
            .store
            .read_events(&self.session_id, after_seq, limit)
            .await?
            .into_iter()
            .filter(|event| event.response_id() == Some(response_id))
            .collect())
    }
}

#[cfg(test)]
mod tests;
