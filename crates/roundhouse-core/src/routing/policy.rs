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

use crate::routing::{Candidate, Decision, RoutingContext, RoutingError, RoutingPolicy};

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

    /// Candidates this policy will score, under both the caller's
    /// entitlements and the deployment's own tuning.
    ///
    /// **The order of the two filters is the blame.** The turn policy runs
    /// first, so an empty set at that point is a refusal *this deployment made
    /// about this tenant* — [`RoutingError::PolicyRefused`], which a retry
    /// cannot fix and only an operator widening a policy can. What survives it
    /// is then filtered by `max_load`, this policy instance's own tuning, and
    /// an empty set at *that* point is a busy fleet —
    /// [`RoutingError::NoViableCandidate`], which the next turn may well not
    /// hit.
    ///
    /// The two are separate on purpose and neither subsumes the other: a
    /// deployment can tune around a busy worker that a tenant is perfectly
    /// entitled to, and a tenant's entitlements say nothing about load. One
    /// combined filter would still route identically and would have exactly
    /// one answer for why it routed nowhere — which is the answer that sends
    /// half the readers to the wrong system.
    fn admissible<'a>(
        &self,
        ctx: &'a RoutingContext<'_>,
    ) -> Result<Vec<&'a Candidate>, RoutingError> {
        let entitled: Vec<&Candidate> = ctx
            .candidates
            .iter()
            .filter(|c| ctx.turn_policy.admits(c, ctx.frontier_history))
            .collect();
        if entitled.is_empty() {
            return Err(RoutingError::PolicyRefused);
        }
        let viable: Vec<&Candidate> = entitled
            .into_iter()
            .filter(|c| match (self.max_load, c.load) {
                (Some(ceiling), Some(load)) => load <= ceiling,
                _ => true,
            })
            .collect();
        if viable.is_empty() {
            return Err(RoutingError::NoViableCandidate);
        }
        Ok(viable)
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
        let pool = self.admissible(ctx)?;

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
        Ok(Decision {
            target: winner.target.clone(),
            rationale: format!(
                "score {:.4} over {} candidate(s); expected prefill {:.0} of {} tokens ({:.0}% cached), ${:.5}",
                best_score,
                pool.len(),
                winner.expected_prefill_tokens,
                ctx.isl_tokens,
                hit_ratio * 100.0,
                winner.expected_cost_usd,
            ),
        })
    }
}

/// Serve most turns from the cheaper pool, but hand every `audit_every`-th turn
/// to the highest-quality target available.
///
/// This is the shape Switchyard's escalation router implements, reproduced
/// natively so the behavior is available without the dependency. The periodic
/// audit is the cheap approximation; the richer version latches to the strong
/// target on a confirmed streak of trouble, which needs a quality signal we do
/// not yet collect.
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
        // past what the caller is entitled to. Unfiltered, this `max_by` would
        // escalate straight through a quality ceiling, an access filter or a
        // spent frontier cadence — none of which it asks about — on every
        // `audit_every`-th turn. The clamp is here rather than in
        // `is_audit_turn` because it *is* still an audit turn: it escalates to
        // the best admissible target, which is what "narrowing clamps the
        // escalation rather than cancelling it" means.
        //
        // The turn policy is the only filter on this branch — it applies no
        // `max_load`, deliberately, since an audit is worth reaching a busy
        // worker for — so an empty set here has exactly one cause and is
        // reported as it: `PolicyRefused`, never the fleet-shaped
        // `NoViableCandidate`.
        let best = ctx
            .candidates
            .iter()
            .filter(|candidate| ctx.turn_policy.admits(candidate, ctx.frontier_history))
            .max_by(|a, b| {
                a.quality_prior
                    .partial_cmp(&b.quality_prior)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or(RoutingError::PolicyRefused)?;

        Ok(Decision {
            target: best.target.clone(),
            rationale: format!(
                "audit turn (every {}); escalated to highest quality prior {:.2}",
                self.audit_every, best.quality_prior
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{FrontierCadence, FrontierHistory, TargetFilter, TurnPolicy};
    use crate::ids::SessionId;
    use crate::routing::{CacheLedger, Target};

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
        Candidate {
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            expected_prefill_tokens: prefill,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 500.0,
            expected_cost_usd: cost,
            quality_prior: 0.95,
            load: None,
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
    }

    impl Fixture {
        /// Open mode: the value every pre-tenancy deployment routes under.
        fn open() -> Self {
            Self {
                session_id: SessionId::new("s"),
                ledger: CacheLedger::new(),
                turn_policy: TurnPolicy::unrestricted(),
                frontier_history: FrontierHistory::default(),
            }
        }

        fn under(turn_policy: TurnPolicy) -> Self {
            Self {
                turn_policy,
                ..Self::open()
            }
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
                Err(RoutingError::NoViableCandidate)
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
        let candidates = mixed_fleet();
        let affinity = AffinityPolicy::new();
        let escalation = EscalationPolicy::new(AffinityPolicy::new(), 4);
        let warm_local = Target::Local {
            worker_id: 2,
            dp_rank: 0,
            model: "llama".into(),
        };
        let scored = "score 0.0000 over 3 candidate(s); expected prefill 500 of 10000 tokens (95% cached), $0.00000";

        for (label, decision, expected) in [
            (
                "affinity, ordinary turn",
                choose(&affinity, &candidates, 1).await,
                Decision {
                    target: warm_local.clone(),
                    rationale: scored.into(),
                },
            ),
            (
                "escalation, ordinary turn",
                choose(&escalation, &candidates, 1).await,
                Decision {
                    target: warm_local.clone(),
                    rationale: scored.into(),
                },
            ),
            (
                "affinity, audit-numbered turn",
                choose(&affinity, &candidates, 4).await,
                Decision {
                    target: warm_local,
                    rationale: scored.into(),
                },
            ),
            (
                "escalation, audit turn",
                choose(&escalation, &candidates, 4).await,
                Decision {
                    target: Target::Frontier {
                        provider: "anthropic".into(),
                        model: "claude".into(),
                    },
                    rationale: "audit turn (every 4); escalated to highest quality prior 0.95"
                        .into(),
                },
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
            Err(RoutingError::NoViableCandidate)
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

    #[test]
    fn normalizing_a_degenerate_set_does_not_divide_by_zero() {
        assert_eq!(normalize(&[5.0, 5.0, 5.0]), vec![0.0, 0.0, 0.0]);
        assert_eq!(normalize(&[]), Vec::<f64>::new());
        assert_eq!(normalize(&[0.0, 10.0]), vec![0.0, 1.0]);
    }
}
