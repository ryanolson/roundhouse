// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Built-in routing policies.
//!
//! These are native implementations behind [`RoutingPolicy`]. Switchyard slots
//! in as a third implementation of the same trait rather than replacing it,
//! which is what keeps a pre-alpha dependency from becoming load-bearing.
//!
//! # What routing our own build taught us
//!
//! Roundhouse was itself built by a multi-model agent system that had to make
//! the same choice this module makes — which model gets which turn — and its
//! working rules are recorded here because they name what the current
//! vocabulary can and cannot express.
//!
//! The rules that worked:
//!
//! - **Route on cost-of-being-wrong, not on task size.** A large mechanical
//!   diff went to a cheap model; a ten-line change to a fencing invariant did
//!   not. The scoring axes below (prefill, dollars, latency) are all *supply*
//!   side; the driver of that decision is a *demand*-side property of the turn
//!   itself — call it stakes.
//! - **Verifiability discounts required quality.** When a turn's output is
//!   cheaply checkable after the fact (tests, review), a mid-tier model plus
//!   verification dominated a top-tier model unverified. When verification is
//!   expensive or impossible — research that gates a design, and reviews
//!   themselves — the strongest model was worth it. This asymmetry is exactly
//!   the shape [`EscalationPolicy`] implements: generation routed down, audits
//!   routed up.
//! - **Contract-defining turns escalate.** Output that becomes other turns'
//!   input spec (plans, protocol designs, review verdicts) amplifies its
//!   errors downstream; those turns always got the strongest model, and the
//!   escalation that mattered was event-driven — at phase boundaries, when
//!   work was about to be committed — not periodic. `audit_every` is the
//!   periodic approximation of that; a boundary signal would be better.
//! - **Escalate on failed verification rather than iterating at the same
//!   tier.** A cheap model that failed its check was not retried; the turn was
//!   re-run one tier up.
//! - **Availability is a routing input, not an exception.** When the preferred
//!   tier returned overload errors twice in a row, the work moved to a
//!   different model rather than queueing behind the outage. This one the
//!   vocabulary below *can* already express: overload is exactly what
//!   `Candidate::load` and `expected_ttft_ms` exist to carry, and a policy
//!   that scores them is already making that call.
//!
//! What the vocabulary is missing to express these: [`RoutingContext`] carries
//! no demand-side signal — no stakes, no verifiability, no "this turn's output
//! closes a task" marker. `quality_prior` and `min_quality` can encode a
//! per-*deployment* floor but not a per-*turn* one. The cheapest honest next
//! step is a client-supplied per-turn quality floor; a turn classifier can
//! come later, and until such a signal exists a stakes-aware policy here would
//! be dead code, so none is shipped.

use async_trait::async_trait;

use crate::routing::{Decision, RoutingContext, RoutingError, RoutingPolicy};

/// Normalize a set of values to 0.0..=1.0 by min-max.
///
/// Prefill tokens, milliseconds, and dollars cannot be summed directly, so each
/// axis is normalized across the candidate set before weighting. A degenerate
/// set (all equal) contributes nothing rather than dividing by zero.
fn normalize(values: &[f64]) -> Vec<f64> {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    if !span.is_finite() || span <= f64::EPSILON {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| (v - min) / span).collect()
}

/// Relative importance of each axis. All weights are non-negative; larger means
/// the axis matters more.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub prefill: f64,
    pub cost: f64,
    pub ttft: f64,
}

impl Default for Weights {
    fn default() -> Self {
        // Prefill-dominant by default: it is both the cheapest signal to get
        // right and the one the rest of the system is built to exploit.
        Self {
            prefill: 1.0,
            cost: 0.5,
            ttft: 0.25,
        }
    }
}

/// Prefer the target with the best expected cache position, subject to load and
/// quality constraints.
pub struct AffinityPolicy {
    weights: Weights,
    /// Skip local candidates whose load exceeds this, in potential prefill
    /// tokens booked on the worker — the unit of [`Candidate::load`], so the
    /// ceiling is a token count and not a utilization fraction. Frontier
    /// candidates report no load and are never excluded by it.
    ///
    /// The whole of this policy instance's own tuning. There used to be a
    /// `min_quality` beside it, with a paragraph explaining how a deployment's
    /// floor and a tenant's floor compose; it had no caller anywhere in the
    /// tree, and the concept belongs to
    /// [`TurnPolicy`](crate::control::TurnPolicy), which is where every floor
    /// a turn is actually subject to now lives. Two knobs spelling one concept
    /// is how a deployment ends up with two answers to "what is the floor".
    max_load: Option<f64>,
}

impl Default for AffinityPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl AffinityPolicy {
    pub fn new() -> Self {
        Self {
            weights: Weights::default(),
            max_load: None,
        }
    }

    pub fn with_weights(mut self, weights: Weights) -> Self {
        self.weights = weights;
        self
    }

    /// Exclude local candidates carrying more than `max_load` potential
    /// prefill tokens.
    pub fn with_max_load(mut self, max_load: f64) -> Self {
        self.max_load = Some(max_load);
        self
    }
}

#[async_trait]
impl RoutingPolicy for AffinityPolicy {
    fn name(&self) -> &str {
        "affinity"
    }

    async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError> {
        if ctx.candidates.is_empty() {
            return Err(RoutingError::NoCandidates);
        }
        // The admissibility resolution, the blame when it comes back empty, and
        // the overflow valve are all one piece of code on the context — see
        // `RoutingContext::admissible`. This policy contributes exactly one
        // thing to it: `max_load`, its own tuning.
        let admitted = ctx.admissible(self.max_load)?;
        let pool = admitted.pool();

        let prefill = normalize(
            &pool
                .iter()
                .map(|c| c.expected_prefill_tokens)
                .collect::<Vec<_>>(),
        );
        let cost = normalize(&pool.iter().map(|c| c.expected_cost_usd).collect::<Vec<_>>());
        let ttft = normalize(&pool.iter().map(|c| c.expected_ttft_ms).collect::<Vec<_>>());

        let mut best_index = 0;
        let mut best_score = f64::INFINITY;
        for index in 0..pool.len() {
            let score = self.weights.prefill * prefill[index]
                + self.weights.cost * cost[index]
                + self.weights.ttft * ttft[index];
            if score < best_score {
                best_score = score;
                best_index = index;
            }
        }

        let winner = pool[best_index];
        let hit_ratio = winner.cache_hit_ratio(ctx.isl_tokens);
        // **No price in this string, and that is a rule about where prices may
        // travel rather than a formatting preference.** A rationale is not a
        // private note: `engine.rs` copies it into `DecisionRecord::rationale`
        // verbatim and the MCP surface's `explain_last_route` copies *that* into
        // a tool result verbatim, so every term here lands in the calling
        // model's own context. A per-turn dollar figure there lets an agent
        // price the whole fleet by alternating `prefer local` / `prefer
        // frontier` with `explain_last_route` — the family-bias leak
        // `StatusResponse`'s "names, never prices" rule exists to prevent — and
        // it argues with a component that cannot check whether the agent is
        // quoting its own context back at it.
        //
        // Nothing that legitimately needs the number loses it: the winner's
        // `expected_cost_usd` is a structured field on the same
        // `DecisionRecord`, beside `rate_card`, where the metrics fold and an
        // operator's dashboard read it and a tool result does not.
        Ok(admitted.decide(
            winner.target.clone(),
            format!(
                "score {:.4} over {} candidate(s); expected prefill {:.0} of {} tokens ({:.0}% cached)",
                best_score,
                pool.len(),
                winner.expected_prefill_tokens,
                ctx.isl_tokens,
                hit_ratio * 100.0,
            ),
        ))
    }
}

/// Serve most turns from the cheaper pool, but hand every `audit_every`-th turn
/// to the highest-quality target available.
///
/// This is the shape Switchyard's escalation router implements (in
/// `NVIDIA-NeMo/Switchyard`, as `EscalationClassifier` — not in NeMo Relay's
/// deprecated client crate of the same name), reproduced natively so the
/// behavior is available without the dependency. The periodic audit is the
/// cheap approximation; the richer version latches to the strong target on a
/// confirmed streak of trouble. The quality signal that variant needs is no
/// longer uncollected so much as unadopted: Switchyard publishes its judge
/// prompt (`prompts/escalation/prompt.md`, Apache-2.0) and a two-confirmation
/// latch whose outage arm deliberately holds the streak rather than clearing
/// it — an unreachable judge is not evidence the cheap tier is fine. Adopting
/// the prompt without the crate is scheduled synergy work
/// (`agent-docs/synergies/nemo-relay.md` §S5); until then `audit_every` stays, knowing it
/// benchmarks below boundary-triggered review on weak executors.
pub struct EscalationPolicy {
    inner: AffinityPolicy,
    audit_every: u64,
}

impl EscalationPolicy {
    pub fn new(inner: AffinityPolicy, audit_every: u64) -> Self {
        Self {
            inner,
            audit_every: audit_every.max(1),
        }
    }

    fn is_audit_turn(&self, turn_index: u64) -> bool {
        // Never audit the opening turn: there is no history to check yet, and
        // paying frontier prices for turn zero of every session is exactly the
        // cost this design exists to avoid.
        turn_index > 0 && turn_index.is_multiple_of(self.audit_every)
    }
}

#[async_trait]
impl RoutingPolicy for EscalationPolicy {
    fn name(&self) -> &str {
        "escalation"
    }

    async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError> {
        if ctx.candidates.is_empty() {
            return Err(RoutingError::NoCandidates);
        }
        if !self.is_audit_turn(ctx.turn_index) {
            return self.inner.choose(ctx).await;
        }

        // The audit branch is the one place in the router that reaches past
        // what any scoring would pick, so it is the one place that could reach
        // past what the caller is entitled to. Unfiltered, a `max_by` here
        // would escalate straight through a quality ceiling, an access filter,
        // a spent frontier cadence or an exhausted budget — none of which it
        // asks about — on every `audit_every`-th turn. The clamp is here rather
        // than in `is_audit_turn` because it *is* still an audit turn: it
        // escalates to the best admissible target, which is what "narrowing
        // clamps the escalation rather than cancelling it" means.
        //
        // `None` for `max_load`, deliberately: an audit is worth reaching a busy
        // worker for. Everything else — the blame when the set empties, and the
        // overflow valve — is the same code the ordinary branch runs, because
        // whether a turn may be dispatched somewhere is not a question two
        // policies get two answers to.
        let admitted = ctx.admissible(None)?;
        let best = admitted.highest_quality();

        Ok(admitted.decide(
            best.target.clone(),
            format!(
                "audit turn (every {}); escalated to highest quality prior {:.2}",
                self.audit_every, best.quality_prior
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        BudgetState, Exhaustion, FrontierCadence, FrontierHistory, TargetFilter, TurnBudget,
        TurnPolicy,
    };
    use crate::ids::SessionId;
    use crate::routing::{CacheLedger, Candidate, Target};

    /// `load` is in booked prefill tokens, the unit a real fleet reports, so
    /// these numbers are on the same scale an operator would calibrate against.
    fn local(worker_id: u64, prefill: f64, load: f64) -> Candidate {
        Candidate {
            target: Target::Local {
                worker_id,
                dp_rank: 0,
                model: "llama".into(),
            },
            expected_prefill_tokens: prefill,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: 0.0,
            quality_prior: 0.6,
            load: Some(load),
        }
    }

    fn frontier(prefill: f64, cost: f64) -> Candidate {
        frontier_rated(prefill, cost, 0.95)
    }

    /// A hosted candidate whose quality prior is the thing under test.
    ///
    /// Needed because the fleet above is deliberately ordered — every local
    /// worker priors at 0.6 and the hosted model at 0.95 — so no floor can
    /// exclude the hosted half without excluding the local half first, and the
    /// floor half of `overflow_never_relaxes_the_allow_filter_or_floor` would
    /// otherwise be testing `PolicyRefused` instead of the valve.
    fn frontier_rated(prefill: f64, cost: f64, quality_prior: f64) -> Candidate {
        Candidate {
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            expected_prefill_tokens: prefill,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 500.0,
            expected_cost_usd: cost,
            quality_prior,
            load: None,
        }
    }

    /// A budget with nothing left, and a project that asked for the valve.
    fn exhausted(overflow: bool) -> TurnBudget {
        TurnBudget::Granted {
            ceiling_usd: 0.0,
            state: BudgetState::Exhausted,
            on_exhaustion: Exhaustion::DegradeToLocal {
                overflow_when_local_saturated: overflow,
            },
        }
    }

    /// Everything a [`RoutingContext`] needs that is not the candidate set.
    ///
    /// A fixture rather than six arguments: the context borrows all of it, so
    /// the alternative is every test owning six locals whose only job is to
    /// outlive the borrow.
    struct Fixture {
        session_id: SessionId,
        ledger: CacheLedger,
        turn_policy: TurnPolicy,
        frontier_history: FrontierHistory,
        budget: TurnBudget,
    }

    impl Fixture {
        /// Open mode: the value every pre-tenancy deployment routes under.
        fn open() -> Self {
            Self {
                session_id: SessionId::new("s"),
                ledger: CacheLedger::new(),
                turn_policy: TurnPolicy::unrestricted(),
                frontier_history: FrontierHistory::default(),
                budget: TurnBudget::Unlimited,
            }
        }

        fn under(turn_policy: TurnPolicy) -> Self {
            Self {
                turn_policy,
                ..Self::open()
            }
        }

        fn spending(mut self, budget: TurnBudget) -> Self {
            self.budget = budget;
            self
        }

        /// `true` for a routed turn that went to a hosted model.
        fn having_routed(mut self, dispatches: &[bool]) -> Self {
            for &to_frontier in dispatches {
                self.frontier_history.record(&if to_frontier {
                    Target::Frontier {
                        provider: "anthropic".into(),
                        model: "claude".into(),
                    }
                } else {
                    Target::Local {
                        worker_id: 1,
                        dp_rank: 0,
                        model: "llama".into(),
                    }
                });
            }
            self
        }

        fn ctx<'a>(&'a self, candidates: &'a [Candidate], turn: u64) -> RoutingContext<'a> {
            RoutingContext {
                session_id: &self.session_id,
                turn_index: turn,
                isl_tokens: 10_000,
                candidates,
                ledger: &self.ledger,
                turn_policy: &self.turn_policy,
                frontier_history: &self.frontier_history,
                budget: &self.budget,
                // Neither axis exists for these two policies: they score
                // candidates, they do not pick tiers. `stage.rs` has the
                // fixture that fills both in.
                signals: None,
                tiers: None,
            }
        }
    }

    async fn choose(policy: &dyn RoutingPolicy, candidates: &[Candidate], turn: u64) -> Decision {
        let fixture = Fixture::open();
        policy.choose(&fixture.ctx(candidates, turn)).await.unwrap()
    }

    #[tokio::test]
    async fn the_warmest_worker_wins_all_else_equal() {
        let candidates = vec![local(1, 9_000.0, 2_000.0), local(2, 500.0, 2_000.0)];
        let decision = choose(&AffinityPolicy::new(), &candidates, 1).await;
        assert_eq!(decision.target, candidates[1].target);
    }

    #[tokio::test]
    async fn an_overloaded_worker_is_excluded_even_when_warmest() {
        let candidates = vec![local(1, 500.0, 120_000.0), local(2, 9_000.0, 1_000.0)];
        let policy = AffinityPolicy::new().with_max_load(50_000.0);
        let decision = choose(&policy, &candidates, 1).await;
        assert_eq!(decision.target, candidates[1].target);
    }

    #[tokio::test]
    async fn a_free_local_worker_beats_a_paid_frontier_at_equal_prefill() {
        let candidates = vec![local(1, 1_000.0, 2_000.0), frontier(1_000.0, 0.30)];
        let decision = choose(&AffinityPolicy::new(), &candidates, 1).await;
        assert!(decision.target.is_local());
    }

    #[tokio::test]
    async fn a_warm_frontier_beats_a_cold_local_worker() {
        // The whole point of one comparable axis: a large enough cache
        // advantage should pull the turn to the frontier despite it costing
        // real money and having worse TTFT.
        let candidates = vec![local(1, 100_000.0, 20_000.0), frontier(200.0, 0.02)];
        let decision = choose(&AffinityPolicy::new(), &candidates, 1).await;
        assert!(!decision.target.is_local(), "{}", decision.rationale);
    }

    #[tokio::test]
    async fn no_admissible_candidate_is_an_error_not_a_bad_choice() {
        // Every worker is over the ceiling this policy instance was tuned
        // with, and nothing about the caller's entitlements refused anything —
        // so the blame is the fleet's, not the tenant's.
        let candidates = vec![local(1, 500.0, 120_000.0)];
        let policy = AffinityPolicy::new().with_max_load(50_000.0);
        let fixture = Fixture::open();
        assert!(
            matches!(
                policy.choose(&fixture.ctx(&candidates, 1)).await,
                Err(RoutingError::NoViableCandidate { .. })
            ),
            "an overloaded fleet is not a policy refusal: the caller here is \
             under the unrestricted policy and could not have been refused by it"
        );
    }

    #[tokio::test]
    async fn escalation_audits_periodically_but_never_on_the_first_turn() {
        let candidates = vec![local(1, 100.0, 2_000.0), frontier(50_000.0, 5.0)];
        let policy = EscalationPolicy::new(AffinityPolicy::new(), 4);

        assert!(choose(&policy, &candidates, 0).await.target.is_local());
        assert!(choose(&policy, &candidates, 1).await.target.is_local());
        assert!(choose(&policy, &candidates, 3).await.target.is_local());
        // Turn 4 is an audit: the expensive, high-quality target wins despite
        // being far worse on every cost axis.
        assert!(!choose(&policy, &candidates, 4).await.target.is_local());
        assert!(choose(&policy, &candidates, 5).await.target.is_local());
        assert!(!choose(&policy, &candidates, 8).await.target.is_local());
    }

    /// The candidate set every policy test below shares: two local workers of
    /// differing warmth and one hosted model that is warmer still but paid.
    fn mixed_fleet() -> Vec<Candidate> {
        vec![
            local(1, 9_000.0, 2_000.0),
            local(2, 500.0, 2_000.0),
            frontier(4_000.0, 0.25),
        ]
    }

    #[tokio::test]
    async fn an_unrestricted_policy_reproduces_m1_routing_byte_for_byte() {
        // Captured from this tree at 5ca00a9 — the commit before per-principal
        // policy existed — by running these two policies over `mixed_fleet()`
        // and printing the `Decision`. Not recomputed from the format string:
        // a pin that derives its expectation from the code under test pins
        // nothing.
        //
        // This is the compatibility guarantee that lets an operator turn the
        // control plane on without re-routing a single existing workload, and
        // it is the reason `TurnPolicy::unrestricted` exists as a named value
        // rather than as an `Option::None` handled at each call site.
        //
        // **The literal moved once, and only by deletion.** The captured string
        // ended `, $0.00000`; the trailing cost term is gone because a rationale
        // is republished into a model's own context by `explain_last_route` and
        // must carry no per-model price (see `AffinityPolicy::choose`). What the
        // pin is for is unaffected — the *target* each policy picks and the
        // score it picked it on are byte-identical to M1, which is what "no
        // existing workload is re-routed" means. A term added back here would be
        // a price leak and not a formatting change, which is why this literal is
        // still a literal.
        let candidates = mixed_fleet();
        let affinity = AffinityPolicy::new();
        let escalation = EscalationPolicy::new(AffinityPolicy::new(), 4);
        let warm_local = Target::Local {
            worker_id: 2,
            dp_rank: 0,
            model: "llama".into(),
        };
        let scored =
            "score 0.0000 over 3 candidate(s); expected prefill 500 of 10000 tokens (95% cached)";
        // The two fields M10 added, pinned empty on every arm rather than
        // spread through four literals: neither policy here picks a tier, so
        // neither has a runner-up to fall forward to or a source to state, and
        // a decision from one of them that acquired either would be dispatching
        // twice for a turn M1 dispatched once.
        let unstaged = |target: Target, rationale: &str| Decision {
            target,
            rationale: rationale.into(),
            budget_state: BudgetState::Unconstrained,
            fallbacks: Vec::new(),
            source: None,
        };

        for (label, decision, expected) in [
            (
                "affinity, ordinary turn",
                choose(&affinity, &candidates, 1).await,
                unstaged(warm_local.clone(), scored),
            ),
            (
                "escalation, ordinary turn",
                choose(&escalation, &candidates, 1).await,
                unstaged(warm_local.clone(), scored),
            ),
            (
                "affinity, audit-numbered turn",
                choose(&affinity, &candidates, 4).await,
                unstaged(warm_local, scored),
            ),
            (
                "escalation, audit turn",
                choose(&escalation, &candidates, 4).await,
                unstaged(
                    Target::Frontier {
                        provider: "anthropic".into(),
                        model: "claude".into(),
                    },
                    "audit turn (every 4); escalated to highest quality prior 0.95",
                ),
            ),
        ] {
            assert_eq!(decision, expected, "{label} must be byte-identical to M1");
        }
    }

    #[tokio::test]
    async fn a_quality_floor_excludes_a_target_the_default_policy_would_pick() {
        // Worker 2 is the warmest thing on the fleet and free, so the default
        // policy picks it — `the_warmest_worker_wins_all_else_equal` is that
        // same claim without a floor. A floor above the local prior has to
        // exclude it outright rather than merely score it down.
        let candidates = mixed_fleet();
        let fixture = Fixture::under(TurnPolicy {
            min_quality: 0.9,
            ..TurnPolicy::unrestricted()
        });

        let decision = AffinityPolicy::new()
            .choose(&fixture.ctx(&candidates, 1))
            .await
            .unwrap();
        assert!(
            !decision.target.is_local(),
            "a 0.9 floor leaves only the hosted model: {}",
            decision.rationale
        );
        assert_eq!(
            choose(&AffinityPolicy::new(), &candidates, 1).await.target,
            candidates[1].target,
            "the control: with no floor the same set routes local"
        );
    }

    #[tokio::test]
    async fn the_escalation_audit_turn_cannot_escalate_past_the_policy() {
        // The audit branch takes `max_by(quality_prior)` over the whole
        // candidate set, so it is the one place in the router that can reach a
        // target no other path would. A principal restricted to its own fleet
        // must get the best *admissible* target, not the best target.
        let candidates = mixed_fleet();
        let policy = EscalationPolicy::new(AffinityPolicy::new(), 4);
        let fixture = Fixture::under(TurnPolicy {
            allow: TargetFilter::parse(["local/*"]).unwrap(),
            ..TurnPolicy::unrestricted()
        });

        let decision = policy.choose(&fixture.ctx(&candidates, 4)).await.unwrap();
        assert!(
            decision.target.is_local(),
            "the audit clamped to the ceiling instead of escalating past it: {}",
            decision.rationale
        );
        assert!(
            !choose(&policy, &candidates, 4).await.target.is_local(),
            "the control: the same audit turn does escalate when nothing forbids it"
        );
    }

    #[tokio::test]
    async fn an_empty_admissible_set_is_a_policy_refusal_not_a_local_fallback() {
        // A filter that matches nothing routes every turn to a free local
        // worker and looks exactly like a cost win, which is why it fails the
        // turn instead. The startup cross-check against the catalog is what
        // makes this rare; this is what makes it loud when it happens anyway.
        //
        // And it fails as `PolicyRefused` on both policies, ordinary turn and
        // audit turn alike: nothing here is overloaded, nothing was tuned out,
        // and the only thing that emptied the set is the tenant's own filter.
        let candidates = mixed_fleet();
        let fixture = Fixture::under(TurnPolicy {
            allow: TargetFilter::parse(["mistral/*"]).unwrap(),
            ..TurnPolicy::unrestricted()
        });

        for policy in [
            Box::new(AffinityPolicy::new()) as Box<dyn RoutingPolicy>,
            Box::new(EscalationPolicy::new(AffinityPolicy::new(), 4)),
        ] {
            for turn in [1u64, 4u64] {
                assert!(
                    matches!(
                        policy.choose(&fixture.ctx(&candidates, turn)).await,
                        Err(RoutingError::PolicyRefused)
                    ),
                    "{} turn {turn} degraded silently, or blamed the fleet for a \
                     refusal this deployment made",
                    policy.name()
                );
            }
        }
    }

    #[tokio::test]
    async fn a_busy_fleet_and_a_refused_tenant_are_told_apart_on_the_same_candidate_set() {
        // The pair, side by side, because the distinction is only visible as a
        // difference: one candidate set, two ways of emptying it, two
        // different things for an operator to go and look at.
        let overloaded = vec![local(1, 500.0, 120_000.0)];
        let tuned = AffinityPolicy::new().with_max_load(50_000.0);
        let open = Fixture::open();
        assert!(matches!(
            tuned.choose(&open.ctx(&overloaded, 1)).await,
            Err(RoutingError::NoViableCandidate { .. })
        ));

        // The identical worker, well under any ceiling, refused by the tenant's
        // filter instead.
        let idle = vec![local(1, 500.0, 0.0)];
        let filtered = Fixture::under(TurnPolicy {
            allow: TargetFilter::parse(["mistral/*"]).unwrap(),
            ..TurnPolicy::unrestricted()
        });
        assert!(matches!(
            AffinityPolicy::new().choose(&filtered.ctx(&idle, 1)).await,
            Err(RoutingError::PolicyRefused)
        ));

        // And when both would empty it, the tenant's filter is reported: it is
        // the one a retry cannot get past, and the one whose remedy is an edit
        // rather than a wait.
        assert!(
            matches!(
                tuned.choose(&filtered.ctx(&overloaded, 1)).await,
                Err(RoutingError::PolicyRefused)
            ),
            "the refusal a retry cannot fix is the one worth reporting"
        );
    }

    #[tokio::test]
    async fn an_exhausted_cadence_serves_locally_rather_than_failing_the_turn() {
        // The control for the test above, and the distinction decision 7 turns
        // on: an empty admissible set is a misconfiguration and fails, while a
        // spent cadence is the knob working — frontier goes inadmissible and
        // local, still admissible, serves.
        let candidates = mixed_fleet();
        let cadence = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 3,
            }),
            ..TurnPolicy::unrestricted()
        };
        let policy = EscalationPolicy::new(AffinityPolicy::new(), 4);

        let fresh = Fixture::under(cadence.clone());
        assert!(
            !policy
                .choose(&fresh.ctx(&candidates, 4))
                .await
                .unwrap()
                .target
                .is_local(),
            "an unspent window still escalates"
        );

        let spent = Fixture::under(cadence).having_routed(&[true, false]);
        let decision = policy.choose(&spent.ctx(&candidates, 4)).await.unwrap();
        assert!(
            decision.target.is_local(),
            "a spent window serves local: {}",
            decision.rationale
        );
    }

    #[tokio::test]
    async fn a_zero_grant_excludes_frontier_and_admits_local_with_no_special_case() {
        // The whole of degrade-to-local, and the reason it needed no branch:
        // local candidates are priced at zero dollars, so `expected_cost_usd <=
        // ceiling` with a ceiling of zero excludes every hosted option and
        // admits every local one through the same comparison every other
        // candidate goes through.
        //
        // The candidate set is the one from `a_warm_frontier_beats_a_cold_local_worker`,
        // chosen because the router *wants* the hosted model here: it is
        // enormously warmer, and the local worker is cold and loaded. If the
        // budget moves this decision, it is the budget that moved it.
        let candidates = vec![local(1, 100_000.0, 20_000.0), frontier(200.0, 0.02)];
        let policy = AffinityPolicy::new();

        let spent = Fixture::open().spending(exhausted(true));
        let decision = policy.choose(&spent.ctx(&candidates, 1)).await.unwrap();
        assert!(
            decision.target.is_local(),
            "an exhausted budget must serve locally rather than fail or overspend: {}",
            decision.rationale
        );
        assert_eq!(decision.budget_state, BudgetState::Exhausted);
        assert!(
            !decision.budget_state.overflowed(),
            "the local pool served, so no valve opened"
        );
        assert!(
            decision.rationale.contains("over 1 candidate(s)"),
            "the hosted option left the scored pool through the ordinary \
             predicate, not through a special case: {}",
            decision.rationale
        );

        // The control: the identical set under no budget picks the hosted model.
        let unlimited = choose(&policy, &candidates, 1).await;
        assert!(!unlimited.target.is_local(), "{}", unlimited.rationale);
        assert_eq!(unlimited.budget_state, BudgetState::Unconstrained);
    }

    #[tokio::test]
    async fn overflow_readmits_frontier_only_when_local_is_load_rejected() {
        // The probe for the escape valve. The budget is a ceiling on *choice*,
        // not a tourniquet on service: when the pool it degraded to cannot take
        // the turn either, the turn goes back to frontier and the overspend is
        // marked rather than hidden.
        let overloaded = vec![local(1, 100.0, 120_000.0), frontier(50_000.0, 5.0)];
        let tuned = AffinityPolicy::new().with_max_load(50_000.0);
        let spent = Fixture::open().spending(exhausted(true));

        let decision = tuned.choose(&spent.ctx(&overloaded, 1)).await.unwrap();
        assert!(
            !decision.target.is_local(),
            "every local candidate was load-rejected, so the valve had to open: {}",
            decision.rationale
        );
        assert_eq!(
            decision.budget_state,
            BudgetState::ExhaustedOverflow,
            "an overspend past the limit is a marked fact or it is a hidden one"
        );
        assert!(
            decision.rationale.contains("local"),
            "the rationale has to name local saturation, which is the only \
             thing that justifies the overspend: {}",
            decision.rationale
        );

        // The first control: the same budget over a fleet that can serve. The
        // valve is for saturation, not for exhaustion.
        let idle = vec![local(1, 100.0, 1_000.0), frontier(50_000.0, 5.0)];
        let served = tuned.choose(&spent.ctx(&idle, 1)).await.unwrap();
        assert!(served.target.is_local(), "{}", served.rationale);
        assert_eq!(served.budget_state, BudgetState::Exhausted);

        // The second: no local capacity at all is the same saturation. A
        // deployment with no fleet has nowhere to degrade *to*, which is the
        // case the startup check refuses to boot with the valve off.
        let frontier_only = vec![frontier(50_000.0, 5.0)];
        let overflowed = tuned.choose(&spent.ctx(&frontier_only, 1)).await.unwrap();
        assert_eq!(overflowed.budget_state, BudgetState::ExhaustedOverflow);

        // The third: with the valve off, the same saturated fleet fails.
        let valve_off = Fixture::open().spending(exhausted(false));
        assert!(matches!(
            tuned.choose(&valve_off.ctx(&overloaded, 1)).await,
            Err(RoutingError::NoViableCandidate { .. })
        ));

        // And the escalation policy's audit branch reaches the valve through
        // the same code: overflow is a property of the turn, not of one
        // policy's scoring.
        let escalation = EscalationPolicy::new(AffinityPolicy::new().with_max_load(50_000.0), 4);
        let audited = escalation
            .choose(&spent.ctx(&frontier_only, 4))
            .await
            .unwrap();
        assert_eq!(audited.budget_state, BudgetState::ExhaustedOverflow);

        // Its control is subtler than the affinity one and worth pinning
        // rather than leaving to be rediscovered: the audit branch passes no
        // `max_load` on purpose, because an audit is worth reaching a busy
        // worker for. A load-rejected local pool is therefore *not* saturated
        // from where the audit stands, so the same overloaded fleet serves
        // locally and no valve opens.
        let tolerated = escalation.choose(&spent.ctx(&overloaded, 4)).await.unwrap();
        assert!(tolerated.target.is_local(), "{}", tolerated.rationale);
        assert_eq!(tolerated.budget_state, BudgetState::Exhausted);
    }

    #[tokio::test]
    async fn overflow_never_relaxes_the_allow_filter_or_floor() {
        // The valve relaxes exactly one axis. A tenant confined to `local/*`
        // does not acquire a hosted model because its own workers filled up,
        // and neither does one whose quality floor excluded that model — those
        // are entitlements, and a saturated fleet is not an entitlement event.
        let tuned = AffinityPolicy::new().with_max_load(50_000.0);
        let overloaded = vec![local(1, 100.0, 120_000.0), frontier(50_000.0, 5.0)];

        let local_only = Fixture::under(TurnPolicy {
            allow: TargetFilter::parse(["local/*"]).unwrap(),
            ..TurnPolicy::unrestricted()
        })
        .spending(exhausted(true));
        assert!(
            matches!(
                tuned.choose(&local_only.ctx(&overloaded, 1)).await,
                Err(RoutingError::NoViableCandidate { .. })
            ),
            "the valve re-admitted a target the tenant's filter had excluded"
        );

        // The floor, on a hosted model priced below it. The local worker is
        // above the floor, so the set is not empty and this is genuinely the
        // valve being asked to reach past a floor rather than a policy refusal.
        let cheap_frontier = vec![
            local(1, 100.0, 120_000.0),
            frontier_rated(50_000.0, 5.0, 0.3),
        ];
        let floored = Fixture::under(TurnPolicy {
            min_quality: 0.5,
            ..TurnPolicy::unrestricted()
        })
        .spending(exhausted(true));
        assert!(
            matches!(
                tuned.choose(&floored.ctx(&cheap_frontier, 1)).await,
                Err(RoutingError::NoViableCandidate { .. })
            ),
            "the valve re-admitted a target below the tenant's quality floor"
        );

        // The control, and it is what makes both assertions about the *axis*
        // rather than about the valve never firing: drop the floor and the same
        // saturated fleet overflows onto the same hosted model.
        let unfloored = Fixture::open().spending(exhausted(true));
        let decision = tuned
            .choose(&unfloored.ctx(&cheap_frontier, 1))
            .await
            .unwrap();
        assert!(!decision.target.is_local());
        assert_eq!(decision.budget_state, BudgetState::ExhaustedOverflow);
    }

    #[tokio::test]
    async fn a_spent_cadence_is_not_bypassed_by_overflow() {
        // The distinction the valve turns on: a cadence is *policy* — an
        // operator's statement about how often this tenant may reach for a
        // hosted model — and the valve is a *budget* device. A spent cadence
        // therefore stands, and the turn fails fleet-shaped rather than
        // spending a ration the tenant does not have.
        let tuned = AffinityPolicy::new().with_max_load(50_000.0);
        let overloaded = vec![local(1, 100.0, 120_000.0), frontier(50_000.0, 5.0)];
        let cadence = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 1,
                per_turns: 3,
            }),
            ..TurnPolicy::unrestricted()
        };

        let spent = Fixture::under(cadence.clone())
            .spending(exhausted(true))
            .having_routed(&[true]);
        assert!(
            matches!(
                tuned.choose(&spent.ctx(&overloaded, 1)).await,
                Err(RoutingError::NoViableCandidate { .. })
            ),
            "the valve spent a frontier ration the cadence had already used"
        );

        // The control: the identical fleet and budget with the window unspent
        // does overflow, so the assertion above is about the cadence and not
        // about the valve being broken.
        let unspent = Fixture::under(cadence).spending(exhausted(true));
        let decision = tuned.choose(&unspent.ctx(&overloaded, 1)).await.unwrap();
        assert_eq!(decision.budget_state, BudgetState::ExhaustedOverflow);
    }

    #[tokio::test]
    async fn exhausted_plus_saturated_with_overflow_off_blames_the_fleet_and_names_the_budget() {
        // Two facts, one message. The blame is the fleet's — load emptied the
        // local pool, and no tenant policy refused anything — but an operator
        // told only that goes tuning workers without noticing that an exhausted
        // budget had already excluded every hosted candidate that could have
        // taken their place.
        let overloaded = vec![local(1, 500.0, 120_000.0), frontier(4_000.0, 0.25)];
        let tuned = AffinityPolicy::new().with_max_load(50_000.0);

        let spent = Fixture::open().spending(exhausted(false));
        let error = tuned
            .choose(&spent.ctx(&overloaded, 1))
            .await
            .expect_err("a saturated pool with the valve off has nowhere to go");
        assert!(
            matches!(error, RoutingError::NoViableCandidate { .. }),
            "the pool was emptied by load, not by the tenant's policy: {error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("budget"),
            "the coincidence of an exhausted budget has to be visible: {message}"
        );

        // The first control, and the one that makes the failure above about
        // the *budget*: the identical fleet under no budget serves the turn on
        // the hosted candidate. It was there all along; the exhausted budget is
        // what removed it.
        let open = Fixture::open();
        let served = tuned.choose(&open.ctx(&overloaded, 1)).await.unwrap();
        assert!(!served.target.is_local(), "{}", served.rationale);

        // The second: a genuinely fleet-only failure — nothing hosted was
        // quoted at all — still reads exactly as it did before budgets existed.
        // The note is a fact about this deployment's budget, not decoration on
        // every busy fleet.
        let local_only = vec![local(1, 500.0, 120_000.0)];
        let plain = tuned
            .choose(&open.ctx(&local_only, 1))
            .await
            .expect_err("every worker is over the ceiling this policy was tuned with");
        assert_eq!(
            plain.to_string(),
            "no candidate satisfied the routing policy's own constraints",
            "an unbudgeted busy fleet must not grow a budget clause"
        );
    }

    #[test]
    fn normalizing_a_degenerate_set_does_not_divide_by_zero() {
        assert_eq!(normalize(&[5.0, 5.0, 5.0]), vec![0.0, 0.0, 0.0]);
        assert_eq!(normalize(&[]), Vec::<f64>::new());
        assert_eq!(normalize(&[0.0, 10.0]), vec![0.0, 1.0]);
    }
}
