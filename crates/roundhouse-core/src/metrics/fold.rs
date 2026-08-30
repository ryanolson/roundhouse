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

use crate::control::{Billing, PrincipalKey, ProjectId};
use crate::event::{
    Accounting, NotRunReason, PlaceboTiming, SessionEvent, SessionEventKind, Usage,
    ValidationOutcome,
};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::metrics::pricing::TokenShape;
use crate::metrics::{ModelKey, ServingMode};
use crate::routing::PooledUsage;
use crate::validate::Arm;

/// Tokens for one grouping, split by whether the provider counted them.
///
/// Kept apart rather than summed, and this is the whole point: a pot's price is
/// linear in the axes [`PooledUsage`] accumulates, so two accumulators can be
/// priced independently and added at no cost, while one accumulator makes the
/// split unrecoverable the instant the first estimated call lands. Merging first
/// and reporting a call-weighted coverage ratio afterwards does not substitute —
/// a 50%-of-calls ratio is consistent with 95% or 5% of the dollars being
/// measured, because calls differ in size by orders of magnitude.
///
/// Both pots are [`PooledUsage`] rather than [`Usage`], and that is not a
/// spelling: a bare `Usage` sum loses the per-call cache-write decision, so a
/// row pooling one measured and one unmeasured turn priced for less than the two
/// turns did — the rollup's `frontier_spend_usd` against the spend ledger's
/// per-turn dollars, which is M11.0 review finding F2.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Counted {
    /// Tokens the provider itself counted.
    pub(super) reported: PooledUsage,
    /// Tokens Roundhouse counted because the provider did not.
    pub(super) estimated: PooledUsage,
}

impl Counted {
    /// Book one settled call's usage under its own provenance.
    fn add(&mut self, usage: &Usage) {
        match usage.accounting {
            Accounting::Reported => self.reported.add(usage),
            Accounting::Estimated => self.estimated.add(usage),
        }
    }

    fn absorb(&mut self, other: &Counted) {
        self.reported.absorb(&other.reported);
        self.estimated.absorb(&other.estimated);
    }

    /// Both provenances together, for the figures that are about volume rather
    /// than confidence.
    pub(super) fn total(&self) -> PooledUsage {
        let mut total = self.reported.clone();
        total.absorb(&self.estimated);
        total
    }
}

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
    ///
    /// Across both pots below: coverage is a question about the *provider's*
    /// accounting, which is orthogonal to whose money paid for it.
    pub(super) estimated_calls: u64,
    /// Tokens roundhouse may put its rate card against.
    pub(super) billed: Counted,
    /// Tokens measured under a subscription seat, which have no price.
    ///
    /// **Separate from [`Self::billed`] rather than distinguished at read
    /// time**, because the two are added into the same row and the row is what
    /// the dashboard prices: a deployment serving one BYOK project and one
    /// pass-through project on the same model has one row for both, and a
    /// single pot would make the seat's tokens indistinguishable from the ones
    /// a rate card applies to. Priced, they become a bill nobody issued — see
    /// [`SettledSpend`](crate::control::SettledSpend), whose rule the ledger has
    /// kept since M3 and this projection had not.
    ///
    /// A *local* dispatch of a pass-through project lands here too, and that is
    /// deliberate rather than incidental: it bills nothing either way, but what
    /// it displaced was a hosted call the caller's seat would have paid for, so
    /// shadow-pricing it into routing savings credits this deployment with
    /// money it was never going to spend. See
    /// [`Billing::of`](crate::control::Billing::of).
    pub(super) seat: Counted,
    /// Summed over locally-served turns: the cheapest frontier option the
    /// router had quoted at the moment it chose local.
    ///
    /// Only over turns whose decision recorded [`Billing::Billed`]. The figure
    /// is a *saving*, and a saving is money this deployment would otherwise
    /// have spent; the hosted option a pass-through session passed over would
    /// have been billed to the caller's seat, so counting it here credits
    /// roundhouse with somebody else's economy.
    ///
    /// [`Billing::Billed`]: crate::control::Billing::Billed
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
    /// Dispatches to this model that never opened a stream and were fallen
    /// forward from.
    ///
    /// **Marked, never booked, and for [`Self::abandoned_side_calls`]'s exact
    /// reason.** A failed attempt produced no tokens, so folding it as a call
    /// would put a zero-token row on the dashboard — which reads as a free one,
    /// and a provider that 503s every request would make itself look like the
    /// cheapest model in the fleet. It is emphatically *not* added to
    /// [`Self::calls`]: that number is the denominator of every rate here, and
    /// a call that never happened does not belong in it.
    ///
    /// The one number that says a tier's first entry is unreachable while its
    /// fallback quietly carries the traffic — which, without this, looks
    /// identical to a recipe whose first entry was never picked.
    pub(super) failed_attempts: u64,
    /// Summed over this row's calls: what the providers themselves said those
    /// calls cost.
    ///
    /// **Not a part of any dollar figure here and never added into one.** Every
    /// other dollar in this struct is priced from the catalog, which is the
    /// number a savings claim is computed from; this is the external bill that
    /// claim is *checked against*. Summing them would make the reconciliation
    /// view's drift a comparison of a number with itself, which is the one
    /// thing that view exists not to do.
    pub(super) provider_reported_usd: f64,
    /// How many of [`Self::calls`] reported a price at all.
    ///
    /// The discriminator, and it is why the sum above is not published bare: a
    /// row where no provider reported anything and a row where one reported
    /// zero dollars are both `0.0`, and only the second is a figure. Most
    /// providers report nothing, so without this the view would publish a
    /// confident `$0.00` for almost every deployment.
    pub(super) provider_reported_calls: u64,
    /// What this row's turns declared they were talking to.
    ///
    /// Only meaningful on a local row, which is the only place a counterfactual
    /// is priced. See [`DeclaredBaseline`] for why it is a three-state value
    /// rather than a set or a last-write.
    pub(super) declared_baseline: DeclaredBaseline,
}

/// The declared baselines one model row's turns named, collapsed.
///
/// **Three states, not a set and not a last-write**, and the middle option is
/// the one that had to be refused. A row accumulates turns over the reporting
/// window, so "the last baseline seen" would move the *basis* of a published
/// saving as the window filled — and `ShadowPricing`'s own tie-break rule
/// already forbids a correlary that changes between two reads with nothing in
/// the log to explain it. A set would keep the information and then need this
/// same rule to use it.
///
/// So: one distinct name prices the row, and disagreement falls back to
/// inference. A conflicting row is not silently mispriced — the basis it
/// publishes says `inferred`, which is the true statement that no single
/// declaration governs it, and every raw baseline is still on its own decision
/// in the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum DeclaredBaseline {
    /// No turn on this row named one.
    #[default]
    Absent,
    /// Every turn that named one named this.
    One(String),
    /// Two or more turns named different models.
    Conflicting,
}

impl DeclaredBaseline {
    fn observe(&mut self, named: &str) {
        *self = match std::mem::take(self) {
            DeclaredBaseline::Absent => DeclaredBaseline::One(named.to_string()),
            DeclaredBaseline::One(seen) if seen == named => DeclaredBaseline::One(seen),
            DeclaredBaseline::One(_) | DeclaredBaseline::Conflicting => {
                DeclaredBaseline::Conflicting
            }
        };
    }

    fn absorb(&mut self, other: &DeclaredBaseline) {
        match other {
            DeclaredBaseline::Absent => {}
            DeclaredBaseline::One(named) => self.observe(named),
            DeclaredBaseline::Conflicting => *self = DeclaredBaseline::Conflicting,
        }
    }

    /// The one name this row may be priced against, or `None`.
    pub(super) fn resolved(&self) -> Option<&str> {
        match self {
            DeclaredBaseline::One(named) => Some(named),
            DeclaredBaseline::Absent | DeclaredBaseline::Conflicting => None,
        }
    }
}

impl Counters {
    /// Every token this row measured, whoever paid for it.
    ///
    /// The volume answer, and it stays whole: a seat's tokens are as real as a
    /// keyed turn's, so they belong in the token breakdown, in coverage, and in
    /// the traffic shape a correlary is inferred from. It is only the *dollars*
    /// that have to know the difference.
    pub(super) fn total_usage(&self) -> Usage {
        let mut total = self.billed.total();
        total.absorb(&self.seat.total());
        total.tokens().clone()
    }

    /// Tokens the provider counted, across both pots.
    pub(super) fn reported_usage(&self) -> Usage {
        let mut total = self.billed.reported.tokens().clone();
        total.add(self.seat.reported.tokens());
        total
    }

    /// Tokens Roundhouse counted in a silent provider's place, across both.
    pub(super) fn estimated_usage(&self) -> Usage {
        let mut total = self.billed.estimated.tokens().clone();
        total.add(self.seat.estimated.tokens());
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
        self.billed.absorb(&other.billed);
        self.seat.absorb(&other.seat);
        self.quoted_alternative_usd += other.quoted_alternative_usd;
        self.side_calls += other.side_calls;
        self.abandoned_side_calls += other.abandoned_side_calls;
        self.failed_attempts += other.failed_attempts;
        self.provider_reported_usd += other.provider_reported_usd;
        self.provider_reported_calls += other.provider_reported_calls;
        self.declared_baseline.absorb(&other.declared_baseline);
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
    /// Whether roundhouse may price what this dispatch consumes, as the
    /// decision recorded it.
    ///
    /// Read off the `Routed` event and carried to the terminal event, because
    /// that is where tokens arrive and the two events are the two halves of one
    /// turn. Read from anywhere else it would be a second answer to a question
    /// the log has already answered — the same argument the rate card travels
    /// in the log under.
    billing: Billing,
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
    /// Every attributed principal of one project, summed in one pass.
    ///
    /// **A scope rather than a loop over [`Self::Principal`] at the call site**,
    /// and the difference is not performance. A caller that summed one
    /// `Principal` view per *configured* member would report a project's
    /// measured spend as the spend of the members it can currently see — so a
    /// key deleted last week, or a user removed from the project this morning,
    /// would take their traffic out of the project's own reconciliation while
    /// leaving it in the deployment's. The fold knows who actually spent; the
    /// config knows who is currently allowed to. Asking the fold is the only
    /// way to get an answer that still adds up.
    ///
    /// [`PrincipalKey::Unattributed`] belongs to no project and is never
    /// collected here, which is what keeps a project's measured column from
    /// absorbing every log written before the control plane existed.
    Project(&'a ProjectId),
}

impl Scope<'_> {
    /// Whether a fold row keyed by `key` belongs in this scope.
    ///
    /// The one predicate the three arms share, so "which rows are mine" is
    /// decided once rather than restated by every accumulator that walks
    /// [`MetricsFold::by_principal`]. A second spelling is how a project's
    /// tokens and a project's turns come to be summed over two different sets
    /// of principals.
    fn collects(&self, key: &PrincipalKey) -> bool {
        match self {
            Scope::Deployment => true,
            Scope::Principal(scope) => key == *scope,
            // Matched on the `Attributed` arm rather than tested with a
            // projection that could return a project for the other one:
            // unattributed usage has no project, and any answer that gave it
            // one would silently bill somebody for it.
            Scope::Project(project) => {
                matches!(key, PrincipalKey::Attributed { project: owner, .. } if owner == *project)
            }
        }
    }
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
                // Every dispatch this decision made and abandoned, booked
                // against the model it was made *to* rather than against the
                // one that eventually served. A row for kimi that shows four
                // failed attempts and no calls is the whole diagnosis; folding
                // them under sol would say the fallback was flaky.
                //
                // Here rather than at the terminal event, unlike the dispatch
                // itself: an attempt is complete the moment it is recorded, it
                // waits for no usage, and holding it in `pending` would lose
                // every attempt of a turn whose response never terminates.
                for attempt in &decision.attempts {
                    self.by_principal
                        .entry(payer.clone())
                        .or_default()
                        .entry(ModelKey::from_target(&attempt.target))
                        .or_default()
                        .failed_attempts += 1;
                }
                // The counterfactual's name, kept on the row it will price.
                //
                // Local only, because a hosted row *is* the money and has no
                // stand-in to price against — a client that names `sol` on a
                // turn served by `sol` has declared nothing anyone needs. The
                // fold learns it here, at the decision, rather than at the
                // terminal event: a baseline is a fact about the request, and
                // holding it in `pending` would lose it for every turn whose
                // response never terminates.
                if decision.chosen.is_local()
                    && let Some(named) = &decision.declared_baseline
                {
                    self.by_principal
                        .entry(payer.clone())
                        .or_default()
                        .entry(ModelKey::from_target(&decision.chosen))
                        .or_default()
                        .declared_baseline
                        .observe(named);
                }
                self.pending.insert(
                    response_id.clone(),
                    Pending {
                        key: ModelKey::from_target(&decision.chosen),
                        // The road not taken, priced by the router at the
                        // moment it chose. On the record rather than here
                        // because the Relay emission reads the same number per
                        // turn, and a `min` spelled twice would agree until one
                        // copy learned about a new candidate kind.
                        best_frontier_alternative_usd: decision.quoted_frontier_alternative_usd(),
                        billing: decision.billing,
                    },
                );
            }
            SessionEventKind::ResponseCompleted {
                response_id, usage, ..
            }
            | SessionEventKind::ResponseIncomplete {
                response_id, usage, ..
            } => {
                // The turn's own last dead dispatch, booked before anything
                // else in this arm and outside every gate below it.
                //
                // **Unconditionally, and that is the whole of review finding
                // G03.** Every other failed attempt arrives on the `Routed` of
                // the dispatch it caused, above; this one caused none, so it
                // rides the terminal event instead. Gating it on `pending` or
                // on `consumed` — the two guards the settle path below needs —
                // would drop exactly the attempts it exists for, since a
                // dispatch that reached nobody has no usage and a turn whose
                // recipe is exhausted is the case where the last target is the
                // only one the client's error names. A single-provider
                // deployment in an outage reported an empty `failed_attempts`
                // for the whole outage: inverted at the moment it matters.
                if let SessionEventKind::ResponseIncomplete {
                    terminal_attempt: Some(attempt),
                    ..
                } = &event.kind
                {
                    self.by_principal
                        .entry(payer.clone())
                        .or_default()
                        .entry(ModelKey::from_target(&attempt.target))
                        .or_default()
                        .failed_attempts += 1;
                }
                // Settled: this response is nobody's open turn any more.
                if let Some(turn_id) = self.turn_of_response.remove(response_id) {
                    self.response_of_turn.remove(&turn_id);
                }
                // The provider's own figure for this call, accumulated on the
                // row that made it and **never on the row's dollars**. It is
                // the external bill the reconciliation view checks
                // `frontier_spend_usd` against, so adding the two would be the
                // view comparing a number with itself. Booked before the
                // `consumed` gate for the same reason the attempt above is: a
                // provider that reported a price reported one whatever this
                // deployment's own evidence rule makes of the tokens.
                if let SessionEventKind::ResponseCompleted {
                    provider_reported_cost_usd: Some(cost_usd),
                    ..
                } = &event.kind
                    && let Some(pending) = self.pending.get(response_id)
                {
                    let counters = self
                        .by_principal
                        .entry(payer.clone())
                        .or_default()
                        .entry(pending.key.clone())
                        .or_default();
                    counters.provider_reported_usd += cost_usd;
                    counters.provider_reported_calls += 1;
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
                    pending.billing,
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
                // Booked as billed, whatever the checked project's credential
                // mode is, and that is a claim about the judge rather than a
                // default: a side call authenticates on this deployment's own
                // transport with `TurnCredential::Absent` — it never forwards a
                // caller's seat — so the money is roundhouse's and the rate card
                // is the right one to price it with.
                settle(counters, usage, None, Billing::Billed);
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
                        // `Withheld` is exposure without disruption — the
                        // timing fired and the channel refused to act — so it
                        // is a decided, not-run validation like any other and
                        // never an intervention. Counting it would report this
                        // deployment interrupting turns it left alone.
                        if let NotRunReason::PlaceboArm {
                            timing: PlaceboTiming::Intervened,
                        } = reason
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
                rows: Cow::Owned(self.summed_rows(scope)),
                totals: ScopeTotals {
                    // Counted rather than filtered: every session is in this
                    // scope, and `Scope::collects` would agree at the cost of a
                    // hash lookup per session per dashboard poll.
                    sessions: self.watermarks.len(),
                    turns: self.turns_of_principal.values().sum(),
                    first_at_ms: self.window_of_principal.values().map(|(f, _)| *f).min(),
                    last_at_ms: self.window_of_principal.values().map(|(_, l)| *l).max(),
                },
            },
            // Every figure filtered through the *same* predicate the rows were,
            // which is the arm's one real hazard: a project whose tokens came
            // from two members and whose window came from one would report a
            // per-second rate over an interval the traffic did not happen in,
            // and nothing about the number would look wrong.
            Scope::Project(_) => ScopeView {
                rows: Cow::Owned(self.summed_rows(scope)),
                totals: ScopeTotals {
                    sessions: self
                        .watermarks
                        .keys()
                        .filter(|session| scope.collects(&self.principal_for(session)))
                        .count(),
                    turns: self
                        .turns_of_principal
                        .iter()
                        .filter(|(key, _)| scope.collects(key))
                        .map(|(_, turns)| *turns)
                        .sum(),
                    first_at_ms: self
                        .window_of_principal
                        .iter()
                        .filter(|(key, _)| scope.collects(key))
                        .map(|(_, (first, _))| *first)
                        .min(),
                    last_at_ms: self
                        .window_of_principal
                        .iter()
                        .filter(|(key, _)| scope.collects(key))
                        .map(|(_, (_, last))| *last)
                        .max(),
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

    /// Every collected principal's rows added together, which is what a
    /// deployment's — or a project's — row for a model *is*. See
    /// [`Counters::absorb`].
    ///
    /// One pass over the one accumulator, filtered by the scope. Not one
    /// [`Scope::Principal`] view per member summed by the caller: that would
    /// make a project's total a function of who the *config* still lists, and
    /// the whole reason to fold is that the log knows who actually spent.
    fn summed_rows(&self, scope: Scope<'_>) -> BTreeMap<ModelKey, Counters> {
        let mut merged: BTreeMap<ModelKey, Counters> = BTreeMap::new();
        for (owner, rows) in &self.by_principal {
            if !scope.collects(owner) {
                continue;
            }
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
    /// One filtered walk rather than an arm per scope: the direct map lookup a
    /// single principal used to get was O(1) against a table with one entry per
    /// principal, and it was also a second place the answer to "whose rows are
    /// these" was decided.
    pub fn validation_tally(&self, scope: Scope<'_>, arm: Arm) -> ValidationTally {
        let mut total = ValidationTally::default();
        for (owner, arms) in &self.validations {
            if !scope.collects(owner) {
                continue;
            }
            if let Some(tally) = arms.get(&arm) {
                total.absorb(tally);
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

    /// Dispatches abandoned before they opened a stream, per model, for one
    /// scope.
    ///
    /// Per-model rather than summed, unlike [`Self::side_call_tally`], because
    /// this question *is* a per-model one: "which target is failing" is the
    /// whole of what it answers, and a total across the tier would say only
    /// that something is.
    pub fn failed_attempts(&self, scope: Scope<'_>) -> Vec<(ModelKey, u64)> {
        let view = self.view(scope);
        let mut rows: Vec<(ModelKey, u64)> = view
            .rows
            .iter()
            .filter(|(_, counters)| counters.failed_attempts > 0)
            .map(|(key, counters)| (key.clone(), counters.failed_attempts))
            .collect();
        // Stable across polls: a list that reordered between two reads of an
        // unchanged fold would read as movement.
        rows.sort_by(|a, b| (&a.0.provider, &a.0.model).cmp(&(&b.0.provider, &b.0.model)));
        rows
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
fn settle(
    counters: &mut Counters,
    usage: &Usage,
    best_frontier_alternative_usd: Option<f64>,
    billing: Billing,
) {
    counters.calls += 1;
    if usage.accounting == Accounting::Estimated {
        counters.estimated_calls += 1;
    }
    // The one branch that decides which pot a call's tokens land in, so the
    // billed/accounted rule is applied here and nowhere else in this module.
    match billing {
        Billing::Billed => counters.billed.add(usage),
        Billing::AccountedNotBilled => counters.seat.add(usage),
    }
    // A counterfactual is a saving only if the money it stands in for would
    // have been ours — the same predicate the pot above turns on, asked of the
    // road not taken.
    if let Some(alternative) = best_frontier_alternative_usd.filter(|_| billing.is_billable()) {
        counters.quoted_alternative_usd += alternative;
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::control::Principal;
    use crate::event::{Accounting, IncompleteReason};
    use crate::routing::{Candidate, DecisionRecord, Target};
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
            // Not a fifth parameter: no fold test asserts on a cache write yet,
            // and widening this helper would make every existing call site read
            // as a claim about a count none of them is about. A test that needs
            // one writes the field.
            cache_write_tokens: 0,
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
            self.turn_billed(Billing::Billed, response, target, considered, usage)
        }

        /// The same turn, on a project that forwards its caller's seat.
        ///
        /// A separate method rather than a parameter on `turn`, so every
        /// fixture that predates pass-through keeps saying what it said: those
        /// logs are billed logs, and a defaulted argument would make that a
        /// coincidence rather than a statement.
        pub(crate) fn seat_turn(
            &mut self,
            response: &str,
            target: Target,
            considered: Vec<Candidate>,
            usage: Usage,
        ) -> &mut Self {
            self.turn_billed(
                Billing::AccountedNotBilled,
                response,
                target,
                considered,
                usage,
            )
        }

        /// The same turn, on a provider that reports what it charged.
        ///
        /// A separate method for `seat_turn`'s reason: every fixture that
        /// predates the sidecar is a fixture whose upstream said nothing, and
        /// that is a statement about those logs rather than a defaulted
        /// argument.
        pub(crate) fn turn_costing(
            &mut self,
            response: &str,
            target: Target,
            usage: Usage,
            provider_reported_cost_usd: f64,
        ) -> &mut Self {
            self.turn(response, target.clone(), Vec::new(), usage.clone());
            // Rewrite the completion this just wrote rather than duplicating
            // the whole builder: the only difference is the sidecar, and a
            // second copy of the decision record is a second thing to keep in
            // step with the first.
            for event in self.events.iter_mut().rev() {
                if let SessionEventKind::ResponseCompleted {
                    provider_reported_cost_usd: slot,
                    ..
                } = &mut event.kind
                {
                    *slot = Some(provider_reported_cost_usd);
                    break;
                }
            }
            self
        }

        fn turn_billed(
            &mut self,
            billing: Billing,
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
                    payer: Default::default(),
                    billing,
                    budget_draw: None,
                    withheld_providers: Vec::new(),
                    declared_baseline: None,
                    attempts: Vec::new(),
                },
            });
            self.push(SessionEventKind::ResponseCompleted {
                response_id,
                usage,
                provider_reported_cost_usd: None,
                stop_reason: None,
            });
            self
        }

        /// A local turn whose client named what it thought it was talking to.
        fn turn_declaring(
            &mut self,
            response: &str,
            target: Target,
            declared: Option<&str>,
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
                    considered: Vec::new(),
                    turn_policy_digest: String::new(),
                    budget_state: Default::default(),
                    rate_card: None,
                    payer: Default::default(),
                    billing: Billing::Billed,
                    budget_draw: None,
                    withheld_providers: Vec::new(),
                    declared_baseline: declared.map(str::to_string),
                    attempts: Vec::new(),
                },
            });
            self.push(SessionEventKind::ResponseCompleted {
                response_id,
                usage,
                provider_reported_cost_usd: None,
                stop_reason: None,
            });
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
            terminal_attempt: None,
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
            acme[&key].reported_usage().input_tokens,
            1_000,
            "a project is charged for its own turns and no others"
        );

        let unattributed = fold
            .by_principal
            .get(&PrincipalKey::Unattributed)
            .expect("pre-control-plane usage is marked, not dropped");
        assert_eq!(unattributed[&key].reported_usage().input_tokens, 7_000);

        // The deployment total still sees both: marked is not the same as
        // excluded. This is `Counters::absorb` merging two principals' rows for
        // one model, which is the only way a deployment row is ever produced.
        assert_eq!(
            fold.summed_rows(Scope::Deployment)[&key]
                .reported_usage()
                .input_tokens,
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

        // **"never into a project", asked of a project scope.** Everything above
        // this line reads `by_principal` and the deployment total, which is what
        // this test could ask before project scoping existed — and it left the
        // name writing a cheque the body did not cash: widening
        // `Scope::collects` to sweep the marked row into every project passes
        // every assertion above and fails only here.
        assert_eq!(
            fold.view(Scope::Project(&ProjectId::from("acme"))).rows[&key]
                .reported_usage()
                .input_tokens,
            1_000,
            "a project's own scope must not absorb usage nobody can be charged for"
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
        let merged = &fold.summed_rows(Scope::Deployment)[&key];
        assert_eq!(merged.calls, 2);
        assert_eq!(merged.estimated_calls, 1, "one of the two was unreported");
        assert_eq!(merged.reported_usage().input_tokens, 10_000);
        assert_eq!(merged.reported_usage().cached_input_tokens, 8_000);
        assert_eq!(merged.reported_usage().output_tokens, 500);
        assert_eq!(merged.reported_usage().reasoning_tokens, 40);
        assert_eq!(
            merged.estimated_usage().input_tokens,
            3_000,
            "the provenance split has to survive the merge, or a deployment \
             reports as measured what a tenant reports as estimated"
        );
        assert_eq!(merged.estimated_usage().output_tokens, 200);
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
        let claude_row = &fold.summed_rows(Scope::Deployment)[&claude()];
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
        assert_eq!(judge.reported_usage().input_tokens, 4_000);
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
            fold.summed_rows(Scope::Deployment)
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
                reason: NotRunReason::PlaceboArm {
                    timing: PlaceboTiming::Intervened,
                },
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
        assert!(fold.summed_rows(Scope::Deployment).is_empty());
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

    /// A project's rows are exactly the sum of its own principals' rows.
    ///
    /// The property the admin plane's `measured_usd` column rests on, and the
    /// two ways it can be wrong point opposite directions: collect too little
    /// and a project reads as under-spent against a ledger that saw everything;
    /// collect too much and one tenant's reconciliation reports another's
    /// traffic. So the assertion is an equality against the accumulator itself
    /// rather than a bound — over-collection fails it just as loudly as
    /// under-collection.
    #[test]
    fn a_project_view_sums_exactly_its_own_principals_rows() {
        let ada = principal("acme", "ada");
        let bob = principal("acme", "bob");
        let eve = principal("globex", "eve");

        let mut mine = LogBuilder::new("acme/ada/main");
        mine.created(Some(ada.clone()));
        mine.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(1_000, 0, 100, 0),
        );
        let mut colleague = LogBuilder::new("acme/bob/main");
        colleague.created(Some(bob.clone()));
        colleague.turn(
            "r2",
            frontier("anthropic", "claude"),
            vec![],
            usage(2_000, 0, 200, 0),
        );
        let mut neighbour = LogBuilder::new("globex/eve/main");
        neighbour.created(Some(eve.clone()));
        neighbour.turn(
            "r3",
            frontier("anthropic", "claude"),
            vec![],
            usage(9_000, 0, 900, 0),
        );
        // And a log from before the control plane, which belongs to nobody.
        let mut legacy = LogBuilder::new("legacy");
        legacy.turn(
            "r4",
            frontier("anthropic", "claude"),
            vec![],
            usage(5_000, 0, 500, 0),
        );

        let mut fold = MetricsFold::new();
        for log in [&mine, &colleague, &neighbour, &legacy] {
            fold.extend(log.events());
        }

        let acme = ProjectId::from("acme");
        let view = fold.view(Scope::Project(&acme));
        let key = claude();
        assert_eq!(
            view.rows[&key].reported_usage().input_tokens,
            3_000,
            "ada's 1,000 and bob's 2,000, and nothing else"
        );

        // Stated a second way, against the accumulator rather than against a
        // number written out by hand: whatever `by_principal` holds for the two
        // acme rows is what the project view must equal. A fixture whose
        // arithmetic drifted would still be checked.
        let summed: u64 = [&ada, &bob]
            .into_iter()
            .map(|principal| {
                fold.by_principal[&PrincipalKey::from(principal)][&key]
                    .reported_usage()
                    .input_tokens
            })
            .sum();
        assert_eq!(view.rows[&key].reported_usage().input_tokens, summed);

        // The two exclusions, named: the neighbouring project, and the row that
        // belongs to no project at all. Without the second, every project's
        // reconciliation would absorb every log written before the control
        // plane existed — into whichever project happened to be asking.
        assert_eq!(
            fold.view(Scope::Project(&ProjectId::from("globex"))).rows[&key]
                .reported_usage()
                .input_tokens,
            9_000
        );
        let deployment = fold.view(Scope::Deployment);
        assert_eq!(
            deployment.rows[&key].reported_usage().input_tokens,
            17_000,
            "the deployment still sees all four, unattributed included"
        );
        assert!(
            fold.view(Scope::Project(&ProjectId::from("nobody")))
                .rows
                .is_empty(),
            "a project that has never served a turn has no rows, which is an \
             answer rather than a lookup failure"
        );
    }

    /// A project collects a principal its config no longer names.
    ///
    /// The fold has no notion of configuration — that is the point. Nothing in
    /// this test deletes a key, because nothing in this crate could: what it
    /// pins is that the *only* thing deciding whether a row is collected is the
    /// project stamped in the log, so a member whose key was revoked this
    /// morning still appears in the project's measured column. A view that
    /// filtered by anything else would let tidying up tenancy silently move
    /// money out of a project's reconciliation and into the drift.
    #[test]
    fn a_project_view_collects_a_principal_the_config_no_longer_names() {
        let departed = principal("acme", "sam");
        let mut log = LogBuilder::new("acme/sam/main");
        log.created(Some(departed.clone()));
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(4_000, 0, 400, 0),
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());

        let acme = ProjectId::from("acme");
        let view = fold.view(Scope::Project(&acme));
        assert_eq!(
            view.rows[&claude()].reported_usage().input_tokens,
            4_000,
            "the log says acme paid for it, and the log is the record"
        );
        assert_eq!(view.totals.turns, 1);
        assert_eq!(view.totals.sessions, 1);
    }

    /// Every figure on a project view is filtered by the same predicate.
    ///
    /// The failure this pins is the quiet one: a project whose tokens came from
    /// two members and whose *window* came from one reports a per-second rate
    /// over an interval its traffic did not happen in, and the number looks
    /// perfectly plausible. Sessions, turns and both window ends are asserted
    /// together for that reason.
    #[test]
    fn a_project_views_sessions_turns_and_window_cover_every_member() {
        let ada = principal("acme", "ada");
        let bob = principal("acme", "bob");
        let eve = principal("globex", "eve");

        let mut early = LogBuilder::new("acme/ada/main");
        early.created(Some(ada.clone()));
        early.turn("r1", local("llama"), vec![], usage(100, 0, 10, 0));

        // Later in wall-clock time than ada's log: the builder starts every log
        // at the same clock, so bob's is advanced by hand to make "the project's
        // window is the union of its members'" a claim with two distinct ends.
        let mut late = LogBuilder::new("acme/bob/main");
        late.at_ms = 500_000;
        late.created(Some(bob.clone()));
        late.turn("r2", local("llama"), vec![], usage(100, 0, 10, 0));

        let mut neighbour = LogBuilder::new("globex/eve/main");
        neighbour.at_ms = 900_000;
        neighbour.created(Some(eve.clone()));
        neighbour.turn("r3", local("llama"), vec![], usage(100, 0, 10, 0));

        let mut fold = MetricsFold::new();
        for log in [&early, &late, &neighbour] {
            fold.extend(log.events());
        }

        let view = fold.view(Scope::Project(&ProjectId::from("acme")));
        assert_eq!(view.totals.sessions, 2, "ada's and bob's, not eve's");
        assert_eq!(view.totals.turns, 2);
        assert_eq!(
            view.totals.first_at_ms,
            fold.view(Scope::Principal(&PrincipalKey::from(&ada)))
                .totals
                .first_at_ms,
            "the project's window opens with its earliest member's"
        );
        assert_eq!(
            view.totals.last_at_ms,
            fold.view(Scope::Principal(&PrincipalKey::from(&bob)))
                .totals
                .last_at_ms,
            "and closes with its latest member's, not with the first member's"
        );
        assert!(
            view.totals.last_at_ms
                < fold
                    .view(Scope::Principal(&PrincipalKey::from(&eve)))
                    .totals
                    .last_at_ms,
            "and never reaches the neighbouring project's, which would disclose \
             when another tenant was last active"
        );
    }
    /// S4: a row prices against one declared baseline, and refuses to price
    /// against two.
    ///
    /// The middle state is the one that had to be refused. A row accumulates
    /// turns over the reporting window, so a last-write rule would move the
    /// *basis* of a published saving as the window filled — which is the same
    /// instability `ShadowPricing`'s tie-break already forbids.
    #[test]
    fn a_row_prices_against_one_declared_baseline_and_never_against_two() {
        let key = ModelKey::from_target(&local("llama"));

        // Agreement across turns is one declaration.
        let mut log = LogBuilder::new("s1");
        log.created(Some(Principal::new("acme", "ada")));
        log.turn_declaring("r1", local("llama"), Some("big"), usage(1_000, 0, 100, 0));
        log.turn_declaring("r2", local("llama"), Some("big"), usage(1_000, 0, 100, 0));
        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        assert_eq!(
            fold.summed_rows(Scope::Deployment)[&key]
                .declared_baseline
                .resolved(),
            Some("big")
        );

        // Disagreement is none, and the row prices through inference rather
        // than against whichever turn happened to be last.
        let mut log = LogBuilder::new("s2");
        log.created(Some(Principal::new("acme", "ada")));
        log.turn_declaring("r1", local("llama"), Some("big"), usage(1_000, 0, 100, 0));
        log.turn_declaring("r2", local("llama"), Some("small"), usage(1_000, 0, 100, 0));
        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        assert_eq!(
            fold.summed_rows(Scope::Deployment)[&key]
                .declared_baseline
                .resolved(),
            None,
            "two turns naming two models leave no single declaration to publish"
        );

        // A hosted row never carries one: it *is* the money, and there is no
        // counterfactual to name.
        let mut log = LogBuilder::new("s3");
        log.created(Some(Principal::new("acme", "ada")));
        log.turn_declaring(
            "r1",
            frontier("anthropic", "big"),
            Some("big"),
            usage(1_000, 0, 100, 0),
        );
        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        assert_eq!(
            fold.summed_rows(Scope::Deployment)
                [&ModelKey::from_target(&frontier("anthropic", "big"))]
                .declared_baseline
                .resolved(),
            None
        );

        // And the control: a client that named nothing declares nothing, which
        // is what separates "inferred because nobody asked" from "inferred
        // because two clients disagreed".
        let mut log = LogBuilder::new("s4");
        log.created(Some(Principal::new("acme", "ada")));
        log.turn_declaring("r1", local("llama"), None, usage(1_000, 0, 100, 0));
        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        assert_eq!(
            fold.summed_rows(Scope::Deployment)[&key].declared_baseline,
            DeclaredBaseline::Absent
        );
    }
}
