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
//! The seam between the two is one method — [`MetricsFold::view`] — and the
//! [`ScopeView`] it hands back, which is itself the argument that it is a real
//! seam rather than a line drawn through one idea.
//!
//! ## One accumulator
//!
//! Everything is folded per principal, and the deployment's answers are
//! *derived* from those rows rather than accumulated beside them. That is a
//! deliberate trade of a little work per poll for the elimination of a whole
//! class of bug: two accumulators fed at two sites drift the first time one
//! path returns early, silently and permanently, and the drift shows up as a
//! project's bill disagreeing with the deployment's. One accumulator cannot
//! disagree with itself. See [`MetricsFold::by_principal`].

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use crate::control::PrincipalKey;
use crate::event::{
    Accounting, NotRunReason, SessionEvent, SessionEventKind, Usage, ValidationOutcome,
};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::metrics::pricing::TokenShape;
use crate::metrics::{ModelKey, ServingMode};
use crate::routing::DecisionRecord;
use crate::validate::Arm;

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
    /// How many of [`Self::calls`] this deployment made for its own purposes.
    ///
    /// A *subset* of `calls`, not an addition to it: the tokens are in the
    /// usage totals above, because money is money whoever asked for it, and the
    /// dashboard's grand total must stay the sum of its rows. What this adds is
    /// the ability to say which part of a row a client asked for — a judge that
    /// happens to be the same model a conversation used would otherwise be
    /// indistinguishable from the conversation.
    pub(super) side_calls: u64,
    /// Side calls that produced nothing and cost an unknown amount.
    ///
    /// **Deliberately not folded as a zero-token call.** A zero-token call is
    /// indistinguishable from a free one, and the whole reason
    /// [`SideCallAbandoned`](crate::event::SessionEventKind::SideCallAbandoned)
    /// is its own kind is that the vocabulary is free to avoid an ambiguity the
    /// terminal-event vocabulary was not. Counted here so an unaccounted call
    /// is *marked*: a validator that times out on every turn shows up as a
    /// number, not as a dashboard that looks its best when its judge is down.
    pub(super) abandoned_side_calls: u64,
}

impl Counters {
    /// Both provenances together, for the figures that are about volume rather
    /// than confidence.
    pub(super) fn total_usage(&self) -> Usage {
        let mut total = self.reported_usage.clone();
        total.add(&self.estimated_usage);
        total
    }

    /// Add another row into this one, field by field.
    ///
    /// The one definition of what merging two rows means, and the reason a
    /// deployment-wide row can be *derived* from its tenants' rather than
    /// accumulated in parallel with them. Every field has to be listed here or
    /// the deployment quietly under-reports it, which is why this sits beside
    /// the fields rather than in the caller that happens to need it.
    pub(super) fn absorb(&mut self, other: &Counters) {
        self.calls += other.calls;
        self.estimated_calls += other.estimated_calls;
        self.reported_usage.add(&other.reported_usage);
        self.estimated_usage.add(&other.estimated_usage);
        self.quoted_alternative_usd += other.quoted_alternative_usd;
        self.side_calls += other.side_calls;
        self.abandoned_side_calls += other.abandoned_side_calls;
    }
}

/// What one arm's validations came to, for one scope.
///
/// A separate accumulator from [`Counters`], and the separation is the plan's
/// rule made structural: money facts and control facts must not merge. A Shadow
/// run spends real money on a judge and takes no action, and a row that summed
/// the two could not tell it from a Live run that spent the same money and
/// changed the trajectory — which is the one comparison the whole
/// instrumentation exists to make.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidationTally {
    /// Validations decided, whatever came of them.
    pub decided: u64,
    /// Validations that reached a verdict.
    pub judged: u64,
    /// Validations that asked nobody — a spent budget, an unreachable judge, an
    /// unparseable answer, an arm that consults nobody.
    pub not_run: u64,
    /// Validations whose action was actually taken.
    ///
    /// Zero for the Shadow arm by construction, which is what makes "the arm
    /// judged and released unchanged" checkable from the fold rather than only
    /// from the engine.
    pub intervened: u64,
}

impl ValidationTally {
    fn absorb(&mut self, other: &ValidationTally) {
        self.decided += other.decided;
        self.judged += other.judged;
        self.not_run += other.not_run;
        self.intervened += other.intervened;
    }
}

/// Side calls made and abandoned, for one scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SideCallTally {
    /// Calls that answered and were booked.
    pub completed: u64,
    /// Calls that produced nothing and cost an unknown amount.
    pub abandoned: u64,
}

/// A dispatch waiting for its response to terminate.
struct Pending {
    key: ModelKey,
    /// `None` when the chosen target was itself a frontier model, or when no
    /// frontier was quoted at all.
    best_frontier_alternative_usd: Option<f64>,
}

/// Whose numbers a report is about.
///
/// An enum rather than an `Option<&PrincipalKey>`, because `None` reads like a
/// default and this is a choice: the deployment-wide document is what an admin
/// gets, one principal's is what a turn key gets, and the difference between
/// them is what stops a tenant reading its neighbours' traffic.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    Deployment,
    Principal(&'a PrincipalKey),
}

/// Folds session events into token aggregates.
///
/// Idempotent by `(session, seq)`: an event already folded is ignored. That is
/// what lets a live feed and a rebuild-from-log coexist without double
/// counting, which they otherwise would the first time a process replayed a
/// session it had already been watching.
#[derive(Default)]
pub struct MetricsFold {
    /// Counters, split by who paid for them. The only token accumulator here.
    ///
    /// There is no deployment-wide copy beside this one, and its absence is the
    /// design. A second accumulator would have to be written by the same code
    /// path on pain of drift, and "on pain of drift" is a property held by
    /// review rather than by the compiler — the first early return that skipped
    /// one of the two would make a project's bill and the deployment's report
    /// disagree, silently and forever. Deployment answers are summed out of
    /// these rows on the way out instead (see [`Self::view`]), so drift is not
    /// unlikely, it is unrepresentable.
    ///
    /// Keyed by [`PrincipalKey`] rather than by [`Principal`](crate::control::Principal)
    /// so that usage nobody can be charged for still lands somewhere visible.
    by_principal: BTreeMap<PrincipalKey, BTreeMap<ModelKey, Counters>>,
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
    /// Turns admitted, split by who admitted them.
    ///
    /// Per principal for the same reason the counters are: a scoped report that
    /// filtered its rows but kept this number deployment-wide would tell a
    /// tenant how busy its neighbours are, in a field nobody thinks to check.
    turns_of_principal: BTreeMap<PrincipalKey, u64>,
    /// The first and last event time seen for each principal.
    ///
    /// The window every rate on a scoped report is computed over. Deployment
    /// -wide first/last would make a tenant's tokens-per-second read as the
    /// deployment's uptime, and would disclose when anyone else was last
    /// active.
    window_of_principal: BTreeMap<PrincipalKey, (u64, u64)>,
    /// Validations folded per principal and per arm.
    ///
    /// The control accumulator, and deliberately beside [`Self::by_principal`]
    /// rather than inside it. Everything in `by_principal` is money and is
    /// keyed by the model that billed it; a validation bills nothing and is
    /// *about* an arm. Folding the two together would need a model key for a
    /// decision, and the only honest one — the judge's — would attribute the
    /// arm comparison to whichever model happened to be answering.
    validations: BTreeMap<PrincipalKey, BTreeMap<Arm, ValidationTally>>,
}

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

/// One scope's rows and volume figures, resolved together.
///
/// Together rather than by two lookups, because the failure being designed out
/// is a report whose money is scoped and whose session count is not: with one
/// value there is no second call for a caller to forget to scope. The rows are
/// borrowed for a principal and owned for the deployment — the deployment's are
/// summed on demand rather than kept — and [`Cow`] is what lets both answers
/// have one type without copying a tenant's rows to say so.
pub(super) struct ScopeView<'a> {
    pub(super) rows: Cow<'a, BTreeMap<ModelKey, Counters>>,
    pub(super) totals: ScopeTotals,
}

impl ScopeView<'_> {
    /// Traffic shape per hosted model, for inferring correlaries.
    ///
    /// Off this view's own rows, so a scoped report infers its counterfactual
    /// from its own traffic. Reading the deployment's shapes into one tenant's
    /// report would price that tenant's local turns against a similarity
    /// argument built out of somebody else's prompts — a number that moves when
    /// a neighbour's workload changes, which is not a number anyone can defend
    /// on a bill.
    pub(super) fn frontier_shapes(&self) -> HashMap<(String, String), TokenShape> {
        self.rows
            .iter()
            .filter(|(key, _)| key.mode == ServingMode::Frontier)
            .filter_map(|(key, counters)| {
                TokenShape::from_rollup(&counters.total_usage(), counters.calls)
                    .map(|shape| ((key.provider.clone(), key.model.clone()), shape))
            })
            .collect()
    }
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

        // Identity first, above everything that could want it. A session's very
        // first event *is* its `SessionCreated`, and this is what teaches the
        // fold whose the session is; learning it inside the match below, after
        // the window had already been widened, filed that one event under
        // `Unattributed` and started every scoped window one event late.
        //
        // Only ever an insert, never a change: the event is written once into
        // an empty log, so a session cannot be reattributed part-way through
        // and have half its spend land elsewhere.
        if let SessionEventKind::SessionCreated {
            principal: Some(principal),
            ..
        } = &event.kind
        {
            self.principal_of_session
                .insert(event.session_id.clone(), PrincipalKey::from(principal));
        }

        // Resolved once, and widened here rather than after the match: two of
        // the arms below return early, and a terminal event that settles no
        // call — a dispatch that died before reaching the provider — is still
        // an event this principal's window has to reach. Above the match there
        // is no arm that can return past it.
        //
        // Timestamps come from every event, not only the ones that carry
        // tokens: the window a rate is computed over is wall-clock, and a
        // session that opened an hour ago and has yet to complete a turn has
        // still been running for an hour.
        let payer = self.principal_for(&event.session_id);
        let window = self
            .window_of_principal
            .entry(payer.clone())
            .or_insert((event.at_ms, event.at_ms));
        window.0 = window.0.min(event.at_ms);
        window.1 = window.1.max(event.at_ms);

        match &event.kind {
            SessionEventKind::TurnStarted {
                turn_id,
                response_id,
            } => {
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

                settle(
                    self.by_principal
                        .entry(payer)
                        .or_default()
                        .entry(pending.key)
                        .or_default(),
                    usage,
                    pending.best_frontier_alternative_usd,
                );
            }
            // Money this deployment spent on its own behalf, booked under the
            // model that billed it and **never paired with a `Routed`**. There
            // is no dispatch to pair with — the seam that makes side calls sits
            // before `plan` — so the pending map is untouched: a side call must
            // not settle some other response's dispatch, and its own row must
            // not wait for a terminal event that will never come.
            SessionEventKind::SideCallCompleted { target, usage, .. } => {
                let counters = self
                    .by_principal
                    .entry(payer)
                    .or_default()
                    .entry(ModelKey::from_target(target))
                    .or_default();
                settle(counters, usage, None);
                counters.side_calls += 1;
            }
            // Marked, not booked. What it billed upstream is the one thing this
            // deployment does not know, and a zero-token call would read as a
            // free one.
            SessionEventKind::SideCallAbandoned { target, .. } => {
                self.by_principal
                    .entry(payer)
                    .or_default()
                    .entry(ModelKey::from_target(target))
                    .or_default()
                    .abandoned_side_calls += 1;
            }
            SessionEventKind::ValidationDecided { arm, outcome, .. } => {
                let tally = self
                    .validations
                    .entry(payer)
                    .or_default()
                    .entry(*arm)
                    .or_default();
                tally.decided += 1;
                match outcome {
                    ValidationOutcome::Judged { action, .. } => {
                        tally.judged += 1;
                        // The arm decides whether the action happened, which is
                        // why both are on the event. A Shadow run computes an
                        // action and takes none, and a fold that read only the
                        // action would report the control arm intervening.
                        if arm.acts() && action.intervenes() {
                            tally.intervened += 1;
                        }
                    }
                    ValidationOutcome::NotRun { reason } => {
                        tally.not_run += 1;
                        if let NotRunReason::PlaceboArm { intervened: true } = reason
                            && arm.acts()
                        {
                            tally.intervened += 1;
                        }
                    }
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

    /// The rows and volume figures a report is built over.
    ///
    /// The one place a scope is resolved, so a report cannot read the
    /// deployment's rows by forgetting to ask for its own.
    ///
    /// The deployment's answers are all derived here — its rows summed out of
    /// the per-principal ones, its sessions counted off the watermarks, its
    /// turns and window folded over the per-principal maps. That costs an
    /// O(principals x models) pass and an O(sessions) scan per call, which is
    /// per dashboard poll and not per event, and it is the price of the
    /// property in [`Self::by_principal`]: there is nothing for the deployment
    /// figures to drift *from*, because they are a function of the only
    /// accumulator there is.
    pub(super) fn view(&self, scope: Scope<'_>) -> ScopeView<'_> {
        match scope {
            Scope::Deployment => ScopeView {
                rows: Cow::Owned(self.deployment_rows()),
                totals: ScopeTotals {
                    sessions: self.watermarks.len(),
                    turns: self.turns_of_principal.values().sum(),
                    first_at_ms: self.window_of_principal.values().map(|(f, _)| *f).min(),
                    last_at_ms: self.window_of_principal.values().map(|(_, l)| *l).max(),
                },
            },
            Scope::Principal(key) => {
                let window = self.window_of_principal.get(key).copied();
                ScopeView {
                    // A principal with no counters is an ordinary answer — a key
                    // that has never served a turn — not a lookup failure, so
                    // this is an empty map rather than an `Option` every caller
                    // would unwrap into the same thing.
                    rows: match self.by_principal.get(key) {
                        Some(rows) => Cow::Borrowed(rows),
                        None => Cow::Owned(BTreeMap::new()),
                    },
                    totals: ScopeTotals {
                        sessions: self
                            .watermarks
                            .keys()
                            .filter(|session| self.principal_for(session) == *key)
                            .count(),
                        turns: self.turns_of_principal.get(key).copied().unwrap_or(0),
                        first_at_ms: window.map(|(first, _)| first),
                        last_at_ms: window.map(|(_, last)| last),
                    },
                }
            }
        }
    }

    /// Every principal's rows added together, which is what the deployment's
    /// row for a model *is*. See [`Counters::absorb`].
    fn deployment_rows(&self) -> BTreeMap<ModelKey, Counters> {
        let mut merged: BTreeMap<ModelKey, Counters> = BTreeMap::new();
        for rows in self.by_principal.values() {
            for (key, counters) in rows {
                merged.entry(key.clone()).or_default().absorb(counters);
            }
        }
        merged
    }

    /// What one arm's validations came to, in one scope.
    ///
    /// Scoped through the same [`Scope`] the money view uses, so a tenant's
    /// report cannot read the deployment's arm counts by forgetting to narrow.
    pub fn validation_tally(&self, scope: Scope<'_>, arm: Arm) -> ValidationTally {
        let mut total = ValidationTally::default();
        match scope {
            Scope::Deployment => {
                for arms in self.validations.values() {
                    if let Some(tally) = arms.get(&arm) {
                        total.absorb(tally);
                    }
                }
            }
            Scope::Principal(key) => {
                if let Some(tally) = self.validations.get(key).and_then(|arms| arms.get(&arm)) {
                    total.absorb(tally);
                }
            }
        }
        total
    }

    /// Side calls made and abandoned, in one scope.
    ///
    /// Summed across model rows, because the question ("how much of this bill
    /// did the deployment order for itself, and how much of it went nowhere") is
    /// not a per-model one — the judge may move between models without the
    /// answer changing.
    pub fn side_call_tally(&self, scope: Scope<'_>) -> SideCallTally {
        let view = self.view(scope);
        view.rows
            .values()
            .fold(SideCallTally::default(), |mut tally, counters| {
                tally.completed += counters.side_calls;
                tally.abandoned += counters.abandoned_side_calls;
                tally
            })
    }

    /// Turns admitted across the deployment.
    pub fn turns(&self) -> u64 {
        self.turns_of_principal.values().sum()
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

/// Book one settled dispatch into a row.
///
/// A free function rather than an inlined block: it is the definition of what
/// booking a call means, and keeping it named and in one piece is what makes
/// [`Counters::absorb`] — the merge that has to stay in step with it — obviously
/// its counterpart rather than a second, unrelated list of fields.
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::control::Principal;
    use crate::event::{Accounting, IncompleteReason};
    use crate::routing::{Candidate, Target};
    use crate::validate::SteerAction;

    // The fixtures live here, with the fold they build logs for, and are
    // re-used by the snapshot-level tests one module up. Two builders would be
    // two clocks, and a test that compared a window across them would be
    // asserting about the fixtures rather than about the fold.

    pub(crate) fn local(model: &str) -> Target {
        Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: model.into(),
        }
    }

    pub(crate) fn frontier(provider: &str, model: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: model.into(),
        }
    }

    pub(crate) fn candidate(target: Target, cost: f64) -> Candidate {
        Candidate {
            target,
            expected_prefill_tokens: 0.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 0.0,
            expected_cost_usd: cost,
            quality_prior: 0.6,
            load: None,
        }
    }

    pub(crate) fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_tokens: reasoning,
            accounting: Accounting::Reported,
        }
    }

    pub(crate) fn principal(project: &str, user: &str) -> Principal {
        Principal::new(project, user)
    }

    /// A minimal session log: one turn routed to `target` and completed.
    ///
    /// Built by hand rather than by driving the engine so the fold can be
    /// tested against logs the engine cannot currently produce — a provider
    /// that reported nothing, a dispatch that died before sending.
    pub(crate) struct LogBuilder {
        session: SessionId,
        events: Vec<SessionEvent>,
        at_ms: u64,
    }

    impl LogBuilder {
        pub(crate) fn new(session: &str) -> Self {
            Self {
                session: SessionId::new(session),
                events: Vec::new(),
                at_ms: 1_000,
            }
        }

        pub(crate) fn push(&mut self, kind: SessionEventKind) -> &mut Self {
            self.at_ms += 10;
            self.events.push(SessionEvent {
                seq: self.events.len() as u64 + 1,
                session_id: self.session.clone(),
                at_ms: self.at_ms,
                kind,
            });
            self
        }

        pub(crate) fn turn(
            &mut self,
            response: &str,
            target: Target,
            considered: Vec<Candidate>,
            usage: Usage,
        ) -> &mut Self {
            let response_id = ResponseId::new(response);
            self.push(SessionEventKind::TurnStarted {
                turn_id: TurnId::new(format!("turn-{response}")),
                response_id: response_id.clone(),
            });
            self.push(SessionEventKind::Routed {
                response_id: response_id.clone(),
                decision: DecisionRecord {
                    chosen: target,
                    rationale: "test".into(),
                    policy: "test".into(),
                    isl_tokens: usage.input_tokens,
                    expected_prefill_tokens: 0.0,
                    expected_cost_usd: 0.0,
                    considered,
                    turn_policy_digest: String::new(),
                    budget_state: Default::default(),
                    rate_card: None,
                },
            });
            self.push(SessionEventKind::ResponseCompleted { response_id, usage });
            self
        }

        /// Open the log the way the engine does: identity first, at seq 1.
        ///
        /// `None` is what a log written before the control plane looks like —
        /// and so is never calling this at all, which is why both shapes are
        /// exercised below.
        pub(crate) fn created(&mut self, principal: Option<Principal>) -> &mut Self {
            self.push(SessionEventKind::SessionCreated {
                model_policy: "affinity".into(),
                principal,
                arm: None,
            })
        }

        /// A call this deployment made for itself. `None` usage means it was
        /// abandoned.
        pub(crate) fn side_call(&mut self, target: Target, usage: Option<Usage>) -> &mut Self {
            let side_call_id = crate::ids::SideCallId::new(format!("sc_{}", self.events.len()));
            match usage {
                Some(usage) => self.push(SessionEventKind::SideCallCompleted {
                    side_call_id,
                    purpose: crate::event::SideCallPurpose::Validate,
                    target,
                    usage,
                }),
                None => self.push(SessionEventKind::SideCallAbandoned {
                    side_call_id,
                    purpose: crate::event::SideCallPurpose::Validate,
                    target,
                    reason: crate::event::SideCallAbandonReason::DeadlineExceeded,
                }),
            }
        }

        pub(crate) fn validation(&mut self, arm: Arm, outcome: ValidationOutcome) -> &mut Self {
            self.push(SessionEventKind::ValidationDecided {
                validation_id: crate::ids::ValidationId::new(format!("val_{}", self.events.len())),
                trigger: crate::validate::TriggerRecord::new(3, 30_000, Vec::new()),
                arm,
                outcome,
            })
        }

        pub(crate) fn events(&self) -> &[SessionEvent] {
            &self.events
        }
    }

    /// A judged outcome carrying `action`, with the verdict fixed.
    ///
    /// The verdict is not what these tests are about — the arm is — so it is
    /// one shape here rather than a parameter at every call site.
    pub(crate) fn judged(action: SteerAction) -> ValidationOutcome {
        ValidationOutcome::Judged {
            side_call_id: crate::ids::SideCallId::new("sc_j"),
            verdict: crate::validate::Verdict {
                on_track: false,
                confidence: 0.7,
                divergence: Some(crate::validate::Divergence {
                    at_step: 2,
                    description: "the failing test has not been opened".into(),
                }),
                missing_context: None,
            },
            action,
        }
    }

    fn claude() -> ModelKey {
        ModelKey {
            mode: ServingMode::Frontier,
            provider: "anthropic".into(),
            model: "claude".into(),
        }
    }

    /// A terminal event that books nothing still belongs to somebody.
    ///
    /// The fixture is the shape the engine really produces: a dispatch that
    /// died before reaching the provider, so its `ResponseIncomplete` carries
    /// empty usage and settles no call. It is still an event in this session,
    /// at this time, and the deployment's window widens for it. A scoped window
    /// that stops one event short reports a tenant's rate over a shorter
    /// interval than the traffic it is computed from, and does so silently.
    ///
    /// This is the regression test for the widening having once sat *after* the
    /// match, past two early returns.
    #[test]
    fn a_terminal_event_that_settles_nothing_still_widens_the_scoped_window() {
        let ada = principal("acme", "ada");
        let mut log = LogBuilder::new("acme/ada/main");
        log.created(Some(ada.clone()));
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(1_000, 0, 100, 0),
        );
        // No `Routed`, empty usage: both of the terminal arm's early returns
        // are on this path.
        log.push(SessionEventKind::ResponseIncomplete {
            response_id: ResponseId::new("r2"),
            reason: IncompleteReason::UpstreamError,
            usage: Usage::default(),
        });

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        // One principal owns every session in this fold, so the two windows are
        // the same window seen through two scopes.
        let deployment = fold.view(Scope::Deployment).totals;
        let scoped = fold
            .view(Scope::Principal(&PrincipalKey::from(&ada)))
            .totals;
        assert_eq!(scoped.first_at_ms, deployment.first_at_ms);
        assert_eq!(
            scoped.last_at_ms, deployment.last_at_ms,
            "the sole principal's window must reach the deployment's last event"
        );
        // And the settled-nothing event really is the last one, or the fixture
        // could pass without exercising anything.
        let last = log.events().last().expect("the log is non-empty").at_ms;
        assert_eq!(scoped.last_at_ms, Some(last));
    }

    /// The window is widened before identity is known only for a log that never
    /// declares one — never for a log whose first event declares it.
    ///
    /// The control for the test above: it pins the ordering from the other
    /// side, so hoisting the widening above the identity insert instead of
    /// below it would fail here rather than pass both.
    #[test]
    fn a_sessions_first_event_lands_in_its_own_window_not_the_marked_row() {
        let ada = principal("acme", "ada");
        let mut log = LogBuilder::new("acme/ada/main");
        log.created(Some(ada.clone()));

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        let created_at = log.events()[0].at_ms;
        let scoped = fold
            .view(Scope::Principal(&PrincipalKey::from(&ada)))
            .totals;
        assert_eq!(
            scoped.first_at_ms,
            Some(created_at),
            "the `SessionCreated` is the principal's own first event"
        );
        let marked = fold
            .view(Scope::Principal(&PrincipalKey::Unattributed))
            .totals;
        assert_eq!(
            marked.first_at_ms, None,
            "nothing in this log is unattributed, so the marked row has no window at all"
        );
    }

    #[test]
    fn unattributed_usage_is_folded_under_its_own_key_and_never_into_a_project() {
        let mut attributed = LogBuilder::new("s1");
        attributed.created(Some(principal("acme", "ada")));
        attributed.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(1_000, 0, 100, 0),
        );

        // A session from before the control plane: no `SessionCreated` at all.
        // Its tokens are real and have to be counted somewhere, but nobody can
        // say whose they were.
        let mut legacy = LogBuilder::new("s2");
        legacy.turn(
            "r2",
            frontier("anthropic", "claude"),
            vec![],
            usage(7_000, 0, 700, 0),
        );

        let mut fold = MetricsFold::new();
        fold.extend(attributed.events());
        fold.extend(legacy.events());

        let key = claude();
        let acme = fold
            .by_principal
            .get(&PrincipalKey::from(&principal("acme", "ada")))
            .expect("the attributed session has a row");
        assert_eq!(
            acme[&key].reported_usage.input_tokens, 1_000,
            "a project is charged for its own turns and no others"
        );

        let unattributed = fold
            .by_principal
            .get(&PrincipalKey::Unattributed)
            .expect("pre-control-plane usage is marked, not dropped");
        assert_eq!(unattributed[&key].reported_usage.input_tokens, 7_000);

        // The deployment total still sees both: marked is not the same as
        // excluded. This is `Counters::absorb` merging two principals' rows for
        // one model, which is the only way a deployment row is ever produced.
        assert_eq!(
            fold.deployment_rows()[&key].reported_usage.input_tokens,
            8_000
        );
        assert_eq!(
            fold.by_principal.keys().cloned().collect::<Vec<_>>(),
            vec![
                PrincipalKey::from(&principal("acme", "ada")),
                PrincipalKey::Unattributed,
            ],
            "the marked row is a row a reader can see the size of, not a silent remainder"
        );
    }

    /// Every field of `Counters` survives the merge.
    ///
    /// `absorb` is a hand-written list of fields, which is the one shape that
    /// fails by omission rather than by compile error: a field added to
    /// `Counters` and forgotten here reads as zero on the deployment's row
    /// while the tenant's is correct. Two principals whose rows differ in every
    /// field is what makes a dropped one visible.
    #[test]
    fn merging_two_principals_rows_carries_every_counter() {
        let mut ada = LogBuilder::new("s1");
        ada.created(Some(principal("acme", "ada")));
        ada.turn(
            "r1",
            local("llama"),
            vec![candidate(frontier("anthropic", "claude"), 0.042)],
            usage(10_000, 8_000, 500, 40),
        );

        let mut bo = LogBuilder::new("s2");
        bo.created(Some(principal("acme", "bo")));
        bo.turn(
            "r2",
            local("llama"),
            vec![candidate(frontier("anthropic", "claude"), 0.008)],
            Usage {
                accounting: Accounting::Estimated,
                ..usage(3_000, 1_000, 200, 0)
            },
        );

        let mut fold = MetricsFold::new();
        fold.extend(ada.events());
        fold.extend(bo.events());

        let key = ModelKey {
            mode: ServingMode::Local,
            provider: crate::metrics::LOCAL_PROVIDER.into(),
            model: "llama".into(),
        };
        let merged = &fold.deployment_rows()[&key];
        assert_eq!(merged.calls, 2);
        assert_eq!(merged.estimated_calls, 1, "one of the two was unreported");
        assert_eq!(merged.reported_usage.input_tokens, 10_000);
        assert_eq!(merged.reported_usage.cached_input_tokens, 8_000);
        assert_eq!(merged.reported_usage.output_tokens, 500);
        assert_eq!(merged.reported_usage.reasoning_tokens, 40);
        assert_eq!(
            merged.estimated_usage.input_tokens, 3_000,
            "the provenance split has to survive the merge, or a deployment \
             reports as measured what a tenant reports as estimated"
        );
        assert_eq!(merged.estimated_usage.output_tokens, 200);
        assert!((merged.quoted_alternative_usd - 0.05).abs() < 1e-12);
        assert_eq!(
            (merged.side_calls, merged.abandoned_side_calls),
            (0, 0),
            "neither tenant made one; the fields are here so the *next* field \
             added to `Counters` fails this test rather than the dashboard"
        );

        // And the same merge with side calls on both sides, so the two new
        // fields are proven to survive it rather than merely proven to be zero.
        let mut judged = LogBuilder::new("s3");
        judged.created(Some(principal("acme", "cy")));
        judged.side_call(
            frontier("anthropic", "claude"),
            Some(usage(4_000, 0, 40, 0)),
        );
        judged.side_call(frontier("anthropic", "claude"), None);
        fold.extend(judged.events());
        let claude_row = &fold.deployment_rows()[&claude()];
        assert_eq!(
            (claude_row.side_calls, claude_row.abandoned_side_calls),
            (1, 1)
        );
    }

    /// A side call is money, and it books like money — under the model that
    /// billed it, with no dispatch to pair with.
    #[test]
    fn a_side_call_books_under_its_own_model_row_and_pairs_with_no_dispatch() {
        let ada = principal("acme", "ada");
        let mut log = LogBuilder::new("acme/ada/main");
        log.created(Some(ada.clone()));
        // One ordinary turn served locally, then a judge consulted about it.
        log.turn("r1", local("llama"), vec![], usage(10_000, 0, 500, 0));
        log.side_call(
            frontier("anthropic", "claude"),
            Some(usage(4_000, 0, 40, 0)),
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        let key = PrincipalKey::from(&ada);
        let rows = fold.by_principal.get(&key).expect("the tenant has rows");
        let judge = &rows[&claude()];
        assert_eq!(judge.calls, 1, "the judge's call is a call");
        assert_eq!(judge.side_calls, 1, "and it is one of ours");
        assert_eq!(judge.reported_usage.input_tokens, 4_000);
        let worker = &rows[&ModelKey {
            mode: ServingMode::Local,
            provider: crate::metrics::LOCAL_PROVIDER.into(),
            model: "llama".into(),
        }];
        assert_eq!(
            (worker.calls, worker.side_calls),
            (1, 0),
            "the turn's own row is untouched: a side call must not settle a \
             dispatch it had nothing to do with"
        );

        // The dashboard total is still the sum of its rows, exactly once.
        assert_eq!(
            fold.deployment_rows()
                .values()
                .map(|row| row.total_usage().total())
                .sum::<u64>(),
            10_500 + 4_040
        );
        assert_eq!(
            fold.pending_dispatches(),
            0,
            "and nothing is left waiting: a side call opens no pending dispatch, \
             so its row does not hang on a terminal event that never comes"
        );
        assert_eq!(
            fold.side_call_tally(Scope::Principal(&key)),
            SideCallTally {
                completed: 1,
                abandoned: 0
            }
        );
    }

    /// The ambiguity this vocabulary is free to avoid, avoided.
    #[test]
    fn an_abandoned_side_call_is_distinct_from_one_that_billed_nothing() {
        let ada = principal("acme", "ada");
        let mut log = LogBuilder::new("acme/ada/main");
        log.created(Some(ada.clone()));
        // A judge that answered and genuinely billed nothing — a cached or
        // free tier — and a judge that timed out. The old vocabulary would have
        // written both as a zero-usage completion and the `consumed` heuristic
        // would have had to guess between them.
        log.side_call(frontier("anthropic", "claude"), Some(usage(0, 0, 0, 0)));
        log.side_call(frontier("anthropic", "claude"), None);

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        let key = PrincipalKey::from(&ada);
        let row = &fold.by_principal[&key][&claude()];
        assert_eq!(
            (row.calls, row.side_calls, row.abandoned_side_calls),
            (1, 1, 1),
            "one call happened and billed nothing; one is unaccounted. A single \
             counter would report two free calls, and a dashboard looks its best \
             exactly when its judge is down"
        );
        assert_eq!(
            fold.side_call_tally(Scope::Principal(&key)),
            SideCallTally {
                completed: 1,
                abandoned: 1
            }
        );
        assert_eq!(
            fold.side_call_tally(Scope::Deployment),
            fold.side_call_tally(Scope::Principal(&key)),
            "the sole tenant's tally is the deployment's, as the money rows are"
        );
    }

    /// The comparison the whole design exists to make, at the fold.
    #[test]
    fn a_shadow_run_is_distinguishable_from_a_live_one() {
        let escalate = SteerAction::Escalate {
            turns: 3,
            overrides: crate::validate::EscalationOverrides { min_quality: 0.8 },
        };
        let ada = principal("acme", "ada");
        let mut log = LogBuilder::new("acme/ada/main");
        log.created(Some(ada.clone()));
        // Two sessions' worth of decisions in one log, which is what a fold
        // sees anyway: identical verdicts and identical actions, differing only
        // in the arm.
        log.validation(Arm::Shadow, judged(escalate.clone()));
        log.validation(Arm::Live, judged(escalate));
        log.validation(
            Arm::Placebo,
            ValidationOutcome::NotRun {
                reason: NotRunReason::PlaceboArm { intervened: true },
            },
        );
        log.validation(
            Arm::Live,
            ValidationOutcome::NotRun {
                reason: NotRunReason::JudgeFailed,
            },
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let key = PrincipalKey::from(&ada);

        assert_eq!(
            fold.validation_tally(Scope::Principal(&key), Arm::Shadow),
            ValidationTally {
                decided: 1,
                judged: 1,
                not_run: 0,
                intervened: 0,
            },
            "the observe-only arm computed an action and took none -- which is \
             what makes it the control"
        );
        assert_eq!(
            fold.validation_tally(Scope::Principal(&key), Arm::Live),
            ValidationTally {
                decided: 2,
                judged: 1,
                not_run: 1,
                intervened: 1,
            },
            "the same verdict, acted on; and a failed consult that is a decision \
             too, marked rather than absent"
        );
        assert_eq!(
            fold.validation_tally(Scope::Principal(&key), Arm::Placebo),
            ValidationTally {
                decided: 1,
                judged: 0,
                not_run: 1,
                intervened: 1,
            },
            "an intervention with no verdict behind it, which is the whole of the \
             placebo"
        );

        // Control facts stay out of the money rows: nothing above dispatched or
        // billed anything.
        assert!(fold.deployment_rows().is_empty());
        // And a tenant with no validations reads as none rather than as a
        // lookup failure.
        assert_eq!(
            fold.validation_tally(
                Scope::Principal(&PrincipalKey::from(&principal("other", "bo"))),
                Arm::Live
            ),
            ValidationTally::default()
        );
    }

    #[test]
    fn replaying_the_same_events_leaves_the_fold_unchanged() {
        let mut ada = LogBuilder::new("s1");
        ada.created(Some(principal("acme", "ada")));
        ada.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(10_000, 8_000, 500, 0),
        );
        let mut legacy = LogBuilder::new("s2");
        legacy.turn("r2", local("llama"), vec![], usage(1_000, 0, 100, 0));

        let mut fold = MetricsFold::new();
        fold.extend(ada.events());
        fold.extend(legacy.events());
        let by_principal = fold.by_principal.clone();
        let turns = fold.turns();

        // A restarted process replaying a log it has already been watching,
        // which is the normal case for every session that takes a second turn.
        assert_eq!(fold.extend(ada.events()), 0);
        assert_eq!(fold.extend(legacy.events()), 0);

        assert_eq!(
            fold.by_principal, by_principal,
            "idempotency by (session, seq) has to cover the attributed fold, \
             or a replay doubles what a project is billed"
        );
        assert_eq!(fold.turns(), turns);
    }
}
