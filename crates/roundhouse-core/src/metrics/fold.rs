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

use crate::control::PrincipalKey;
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
    /// The same counters again, split by who paid for them.
    ///
    /// A second grouping of one set of events rather than a second count of
    /// them: every row here is written by the same [`settle`] call that writes
    /// the deployment-wide row, from the same event, inside the same
    /// idempotency gate. That is what lets "the per-principal fold and the
    /// deployment fold sum to the same totals" be asserted by a test instead
    /// of hoped for — two accumulators updated at two sites would drift the
    /// first time one path returned early, and the drift would be silent.
    ///
    /// Keyed by [`PrincipalKey`] rather than by [`Principal`](crate::control::Principal)
    /// so that usage nobody can be charged for still lands somewhere visible.
    pub(super) by_principal: BTreeMap<PrincipalKey, BTreeMap<ModelKey, Counters>>,
    /// Who each session belongs to, learned from its `SessionCreated`.
    ///
    /// Sound because that event is written into an empty log and so carries
    /// seq 1, while a replay starts at seq 0: attribution is therefore known
    /// before any event that could spend money, with no side table, no
    /// secondary index, and no ordering assumption beyond the one the store
    /// already guarantees. A session whose log never named a principal — every
    /// log written before the control plane — simply never gains an entry and
    /// resolves to [`PrincipalKey::Unattributed`].
    ///
    /// Grows with `watermarks` and for the same reason; see the note there.
    principal_of_session: HashMap<SessionId, PrincipalKey>,
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
    /// Turns admitted, split by who admitted them.
    ///
    /// Beside the deployment counter for the same reason `by_principal` sits
    /// beside `models`: a scoped report that filtered its rows but kept this
    /// number deployment-wide would tell a tenant how busy its neighbours are,
    /// in a field nobody thinks to check. See [`Self::totals_for`].
    turns_of_principal: BTreeMap<PrincipalKey, u64>,
    /// The first and last event time seen for each principal.
    ///
    /// The window every rate on a scoped report is computed over. Deployment
    /// -wide first/last would make a tenant's tokens-per-second read as the
    /// deployment's uptime, and would disclose when anyone else was last
    /// active.
    window_of_principal: BTreeMap<PrincipalKey, (u64, u64)>,
    pub(super) first_at_ms: Option<u64>,
    pub(super) last_at_ms: Option<u64>,
}

/// The rows of a fold that is empty for the principal asked about.
///
/// A principal with no counters is an ordinary answer — a key that has never
/// served a turn — not a lookup failure, so [`MetricsFold::rows`] hands back
/// an empty map rather than an `Option` every caller would have to unwrap into
/// the same thing.
static NO_ROWS: BTreeMap<ModelKey, Counters> = BTreeMap::new();

/// The volume figures a snapshot carries that are not per-model.
///
/// A struct rather than a 4-tuple because three of the four are integers and
/// two of those are counts of different things: `(1, 1, Some(10), Some(20))`
/// at a call site is exactly the shape that gets transposed once and never
/// noticed.
pub(super) struct ScopeTotals {
    pub(super) sessions: usize,
    pub(super) turns: u64,
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
                let payer = self.principal_for(&event.session_id);
                *self.turns_of_principal.entry(payer).or_default() += 1;
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

                // Both accumulators, one definition of the arithmetic, and
                // neither reachable without the other: every early return
                // above this point skips both, so there is no path on which
                // one fold books a call the other does not.
                let payer = self.principal_for(&event.session_id);
                let key = pending.key;
                settle(
                    self.models.entry(key.clone()).or_default(),
                    usage,
                    pending.best_frontier_alternative_usd,
                );
                settle(
                    self.by_principal
                        .entry(payer)
                        .or_default()
                        .entry(key)
                        .or_default(),
                    usage,
                    pending.best_frontier_alternative_usd,
                );
            }
            SessionEventKind::SessionCreated { principal, .. } => {
                // Only ever an insert, never a change: the event is written
                // once into an empty log, so a session cannot be reattributed
                // part-way through and have half its spend land elsewhere.
                if let Some(principal) = principal {
                    self.principal_of_session
                        .insert(event.session_id.clone(), PrincipalKey::from(principal));
                }
            }
            SessionEventKind::ItemAppended { .. }
            | SessionEventKind::OutputTextDelta { .. }
            | SessionEventKind::TurnDeduplicated { .. }
            | SessionEventKind::Error { .. } => {}
        }

        // After the match, never before: a session's very first event *is* its
        // `SessionCreated`, and the arm above is what taught this fold whose
        // it is. Widening the window first would file that one event under
        // `Unattributed` and leave every scoped window starting one event late.
        let payer = self.principal_for(&event.session_id);
        let window = self
            .window_of_principal
            .entry(payer)
            .or_insert((event.at_ms, event.at_ms));
        window.0 = window.0.min(event.at_ms);
        window.1 = window.1.max(event.at_ms);
        true
    }

    /// Fold a batch, returning how many were new.
    pub fn extend<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent>) -> usize {
        events.into_iter().filter(|e| self.apply(e)).count()
    }

    /// Who a session's spend belongs to.
    ///
    /// Total by construction: a log that never named a principal is not an
    /// error and not a lookup failure, it is the marked row.
    fn principal_for(&self, session_id: &SessionId) -> PrincipalKey {
        self.principal_of_session
            .get(session_id)
            .cloned()
            .unwrap_or(PrincipalKey::Unattributed)
    }

    /// Every principal this fold has counters for, marked row included.
    ///
    /// The public half of the per-principal seam: it names the rows a scoped
    /// report would be built over. The counters themselves stay behind
    /// `pub(super)`, reached the same way the deployment-wide rows are — see
    /// `by_principal` — because [`Counters`] is money-free by design and
    /// pricing happens in [`snapshot`](super::snapshot), on the way out, and
    /// nowhere else.
    pub fn principals(&self) -> Vec<PrincipalKey> {
        self.by_principal.keys().cloned().collect()
    }

    /// The counter rows a report is built over: the deployment's, or one
    /// principal's.
    ///
    /// The one place the two are chosen between, so a scoped report cannot
    /// read the deployment's rows by forgetting to ask.
    pub(super) fn rows(&self, scope: Option<&PrincipalKey>) -> &BTreeMap<ModelKey, Counters> {
        match scope {
            None => &self.models,
            Some(key) => self.by_principal.get(key).unwrap_or(&NO_ROWS),
        }
    }

    /// The volume and window figures for the same scope.
    ///
    /// Sessions are counted by scanning the watermarks through
    /// [`Self::principal_for`] rather than kept as a fourth per-principal map:
    /// the answer is already implied by state the fold must keep anyway, and a
    /// separate counter would be a second thing to keep in agreement with it —
    /// exactly the drift `by_principal` was shaped to avoid. The scan is
    /// O(sessions) once per request against a dashboard poll, not per event.
    pub(super) fn totals_for(&self, scope: Option<&PrincipalKey>) -> ScopeTotals {
        match scope {
            None => ScopeTotals {
                sessions: self.watermarks.len(),
                turns: self.turns,
                first_at_ms: self.first_at_ms,
                last_at_ms: self.last_at_ms,
            },
            Some(key) => {
                let window = self.window_of_principal.get(key).copied();
                ScopeTotals {
                    sessions: self
                        .watermarks
                        .keys()
                        .filter(|session| self.principal_for(session) == *key)
                        .count(),
                    turns: self.turns_of_principal.get(key).copied().unwrap_or(0),
                    first_at_ms: window.map(|(first, _)| first),
                    last_at_ms: window.map(|(_, last)| last),
                }
            }
        }
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

}

/// Traffic shape per hosted model, for inferring correlaries.
///
/// Takes the rows rather than the whole fold so a scoped report infers its
/// counterfactual from its own traffic. Reading the deployment's shapes into
/// one tenant's report would price that tenant's local turns against a
/// similarity argument built out of somebody else's prompts — a number that
/// moves when a neighbour's workload changes, which is not a number anyone can
/// defend on a bill.
pub(super) fn frontier_shapes(
    rows: &BTreeMap<ModelKey, Counters>,
) -> HashMap<(String, String), TokenShape> {
    rows.iter()
        .filter(|(key, _)| key.mode == ServingMode::Frontier)
        .filter_map(|(key, counters)| {
            TokenShape::from_rollup(&counters.total_usage(), counters.calls)
                .map(|shape| ((key.provider.clone(), key.model.clone()), shape))
        })
        .collect()
}

/// Book one settled dispatch into a row.
///
/// A free function with one caller-visible definition, called once per
/// accumulator. It exists precisely so the deployment-wide fold and the
/// per-principal fold cannot say different things about the same call: the
/// alternative — two inlined copies of this arithmetic — drifts the first time
/// one of them grows a case the other does not, and the drift shows up as a
/// project's bill quietly disagreeing with the deployment's.
fn settle(counters: &mut Counters, usage: &Usage, best_frontier_alternative_usd: Option<f64>) {
    counters.calls += 1;
    match usage.accounting {
        Accounting::Reported => counters.reported_usage.add(usage),
        Accounting::Estimated => {
            counters.estimated_calls += 1;
            counters.estimated_usage.add(usage);
        }
    }
    if let Some(alternative) = best_frontier_alternative_usd {
        counters.quoted_alternative_usd += alternative;
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
