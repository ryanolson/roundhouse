// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Events in, counters out.
//!
//! The half of the metrics projection that touches no money. It answers "how
//! many tokens went where", and deliberately stops there: prices are
//! configuration and they change, so folding dollars in here would freeze
//! whatever rate card happened to be loaded when a turn ran and a corrected
//! price would require replaying every session. Tokens are facts, so tokens
//! are what is accumulated. [`super::snapshot`] applies the rate card.
//!
//! The seam between the two is three fields and one method, which is itself
//! the argument that it is a real seam rather than a line drawn through one
//! idea.

use std::collections::{BTreeMap, HashMap};

use crate::event::{Accounting, SessionEvent, SessionEventKind, Usage};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::metrics::pricing::TokenShape;
use crate::metrics::{ModelKey, ServingMode};
use crate::routing::DecisionRecord;

/// Raw counters for one [`ModelKey`], before any rate card is applied.
///
/// Deliberately money-free. Prices are configuration and they change; folding
/// dollars in here would freeze whatever rate card happened to be loaded when a
/// turn ran, and a corrected price would then require replaying every session.
/// Tokens are facts, so tokens are what is accumulated.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Counters {
    pub(super) calls: u64,
    /// Calls whose usage the provider never reported. See [`Accounting`].
    pub(super) estimated_calls: u64,
    /// Tokens the provider itself counted.
    pub(super) reported_usage: Usage,
    /// Tokens Roundhouse counted because the provider did not.
    ///
    /// Kept apart from `reported_usage` rather than summed into it, and this is
    /// the whole point: pricing is linear in tokens, so two accumulators can be
    /// priced independently at no cost, while one accumulator makes the split
    /// unrecoverable the instant the first estimated call lands. Merging first
    /// and reporting a call-weighted coverage ratio afterwards does not
    /// substitute — a 50%-of-calls ratio is consistent with 95% or 5% of the
    /// dollars being measured, because calls differ in size by orders of
    /// magnitude.
    pub(super) estimated_usage: Usage,
    /// Summed over locally-served turns: the cheapest frontier option the
    /// router had quoted at the moment it chose local.
    pub(super) quoted_alternative_usd: f64,
}

impl Counters {
    /// Both provenances together, for the figures that are about volume rather
    /// than confidence.
    pub(super) fn total_usage(&self) -> Usage {
        let mut total = self.reported_usage.clone();
        total.add(&self.estimated_usage);
        total
    }
}

/// A dispatch waiting for its response to terminate.
struct Pending {
    key: ModelKey,
    /// `None` when the chosen target was itself a frontier model, or when no
    /// frontier was quoted at all.
    best_frontier_alternative_usd: Option<f64>,
}

/// Folds session events into token and dollar aggregates.
///
/// Idempotent by `(session, seq)`: an event already folded is ignored. That is
/// what lets a live feed and a rebuild-from-log coexist without double
/// counting, which they otherwise would the first time a process replayed a
/// session it had already been watching.
#[derive(Default)]
pub struct MetricsFold {
    pub(super) models: BTreeMap<ModelKey, Counters>,
    /// Highest sequence number folded per session.
    ///
    /// Gains an entry per session and never loses one, which makes it the
    /// fastest-growing state in this struct — faster than the abandoned-
    /// dispatch residue documented below, since it grows on every session
    /// rather than only on failed ones. It cannot simply be pruned: it *is*
    /// the idempotency guarantee, and forgetting a session means re-folding
    /// its log on the next replay. Bounding it means windowing on event time,
    /// which is a deliberate change to make with the residue, not before it.
    watermarks: HashMap<SessionId, u64>,
    pending: HashMap<ResponseId, Pending>,
    /// The response each open turn is currently on, and the inverse.
    ///
    /// Kept so an abandoned dispatch can be *retired* rather than waited on
    /// forever. A turn whose owner was fenced mid-dispatch never gets a
    /// terminal event — the settle seam's append is best-effort on exactly
    /// that path — but the client's retry re-admits the same `turn_id` under a
    /// fresh `ResponseId`, and a second `TurnStarted` for a turn is positive
    /// proof that the previous response was abandoned. That is a supersession
    /// rule rather than a heuristic, and it is driven entirely off log
    /// contents, so a live fold and a replay still agree.
    ///
    /// Both maps drain: at a terminal event, and at supersession. What they do
    /// not cover is a turn abandoned and then never retried, which stays until
    /// the process ends.
    response_of_turn: HashMap<TurnId, ResponseId>,
    turn_of_response: HashMap<ResponseId, TurnId>,
    turns: u64,
    pub(super) first_at_ms: Option<u64>,
    pub(super) last_at_ms: Option<u64>,
}

impl MetricsFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event. Returns whether it was new.
    pub fn apply(&mut self, event: &SessionEvent) -> bool {
        let watermark = self.watermarks.entry(event.session_id.clone()).or_default();
        if event.seq <= *watermark {
            return false;
        }
        *watermark = event.seq;

        // Timestamps come from every event, not only the ones that carry
        // tokens: the window a rate is computed over is wall-clock, and a
        // session that opened an hour ago and has yet to complete a turn has
        // still been running for an hour.
        self.first_at_ms = Some(
            self.first_at_ms
                .map_or(event.at_ms, |at| at.min(event.at_ms)),
        );
        self.last_at_ms = Some(
            self.last_at_ms
                .map_or(event.at_ms, |at| at.max(event.at_ms)),
        );

        match &event.kind {
            SessionEventKind::TurnStarted {
                turn_id,
                response_id,
            } => {
                self.turns += 1;
                // A second start for this turn means the first response will
                // never terminate. Retire it now rather than hold it forever.
                if let Some(abandoned) = self
                    .response_of_turn
                    .insert(turn_id.clone(), response_id.clone())
                {
                    self.pending.remove(&abandoned);
                    self.turn_of_response.remove(&abandoned);
                }
                self.turn_of_response
                    .insert(response_id.clone(), turn_id.clone());
            }
            SessionEventKind::Routed {
                response_id,
                decision,
            } => {
                self.pending.insert(
                    response_id.clone(),
                    Pending {
                        key: ModelKey::from_target(&decision.chosen),
                        best_frontier_alternative_usd: best_frontier_alternative(decision),
                    },
                );
            }
            SessionEventKind::ResponseCompleted { response_id, usage }
            | SessionEventKind::ResponseIncomplete {
                response_id, usage, ..
            } => {
                // Settled: this response is nobody's open turn any more.
                if let Some(turn_id) = self.turn_of_response.remove(response_id) {
                    self.response_of_turn.remove(&turn_id);
                }
                let Some(pending) = self.pending.remove(response_id) else {
                    return true;
                };
                // The same evidence rule the cache ledger uses, and for the
                // same reason. A completion always consumed tokens; an
                // incomplete only did if it reports billed input, because the
                // engine also terminates dispatches that failed before
                // anything reached the provider and those carry empty usage.
                // Counting one of those would add a call that never happened
                // to the denominator of every rate on the dashboard.
                let consumed = matches!(event.kind, SessionEventKind::ResponseCompleted { .. })
                    || usage.input_tokens > 0;
                if !consumed {
                    return true;
                }

                let counters = self.models.entry(pending.key).or_default();
                counters.calls += 1;
                match usage.accounting {
                    Accounting::Reported => counters.reported_usage.add(usage),
                    Accounting::Estimated => {
                        counters.estimated_calls += 1;
                        counters.estimated_usage.add(usage);
                    }
                }
                if let Some(alternative) = pending.best_frontier_alternative_usd {
                    counters.quoted_alternative_usd += alternative;
                }
            }
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ItemAppended { .. }
            | SessionEventKind::OutputTextDelta { .. }
            | SessionEventKind::TurnDeduplicated { .. }
            | SessionEventKind::Error { .. } => {}
        }
        true
    }

    /// Fold a batch, returning how many were new.
    pub fn extend<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent>) -> usize {
        events.into_iter().filter(|e| self.apply(e)).count()
    }

    pub fn sessions(&self) -> usize {
        self.watermarks.len()
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// Dispatches that were routed but whose response has not terminated.
    ///
    /// Observability for the size of the pending map, so a test can assert on
    /// what the fold is still holding rather than only on what it has counted.
    pub fn pending_dispatches(&self) -> usize {
        self.pending.len()
    }

    /// Turns admitted whose response has not terminated.
    ///
    /// Companion to [`Self::pending_dispatches`]: a turn appears here from its
    /// `TurnStarted` and leaves at its terminal event or when a retry
    /// supersedes it, so the two counts drain together.
    pub fn open_turns(&self) -> usize {
        self.response_of_turn.len()
    }

    /// Traffic shape per hosted model, for inferring correlaries.
    pub(super) fn frontier_shapes(&self) -> HashMap<(String, String), TokenShape> {
        self.models
            .iter()
            .filter(|(key, _)| key.mode == ServingMode::Frontier)
            .filter_map(|(key, counters)| {
                TokenShape::from_rollup(&counters.total_usage(), counters.calls)
                    .map(|shape| ((key.provider.clone(), key.model.clone()), shape))
            })
            .collect()
    }
}

/// The cheapest hosted option the router passed over when it chose local.
///
/// Read off the decision's own `considered` list rather than recomputed, so it
/// reflects the ledger state and prices in force at that moment. `None` when
/// local did not win — there is no alternative to a frontier call that was
/// itself the frontier — or when no hosted model was quoted.
fn best_frontier_alternative(decision: &DecisionRecord) -> Option<f64> {
    if !decision.chosen.is_local() {
        return None;
    }
    decision
        .considered
        .iter()
        .filter(|candidate| !candidate.target.is_local())
        .map(|candidate| candidate.expected_cost_usd)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}
