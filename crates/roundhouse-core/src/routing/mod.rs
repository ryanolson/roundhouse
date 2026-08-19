// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Routing: putting local workers and frontier models on one comparable axis.
//!
//! The problem this layer solves is that "serve this turn from our own Llama on
//! worker 7" and "send it to Anthropic" are not obviously comparable. They
//! become comparable once both are expressed as *cache-adjusted expected
//! prefill*, plus latency, price, and a quality prior.
//!
//! The two sides get that number very differently:
//!
//! - **Local** — Dynamo's selection service answers directly. `POST /select` is
//!   query-only and returns `effective_prefill_tokens`, the scheduler's own
//!   cache-credit-weighted prefill cost, without booking anything. That is
//!   already the currency we want.
//! - **Frontier** — no provider exposes its cache, so we *model* it from the
//!   routing ledger: what we last sent to that target, when, and under which
//!   provider TTL. See [`ledger`].
//!
//! Costing happens in `roundhouse-fleet`, which owns the transports. This
//! module owns the vocabulary and the choice.
//!
//! [`RoutingPolicy`] is the seam that keeps Switchyard optional. Switchyard
//! here means the `NVIDIA-NeMo/Switchyard` library (`switchyard-libsy`), not
//! NeMo Relay's deprecated `crates/switchyard` HTTP client. Its `Algorithm`
//! trait is a good fit — the algorithm emits `Step::CallModel` with a semantic
//! target and the *host* executes it — but `libsy::State` is in-memory with no
//! pluggable persistence (re-verified at main `47babb1`, 2026-08-19), which
//! collides with surviving process death. Keeping libsy behind this trait also
//! absorbs its pre-alpha churn: `Algorithm::route`'s return type and the
//! `Step`/`Driver` vocabulary each changed shape during one week of 2026-08.

pub mod ledger;
pub mod policy;

pub use ledger::{CacheLedger, CacheModel, LedgerEntry, ProviderPricing};
pub use policy::{AffinityPolicy, EscalationPolicy};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::control::{BudgetState, FrontierHistory, Payer, TurnBudget, TurnPolicy};
use crate::ids::SessionId;

/// Where a turn can be sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A worker in the local Dynamo fleet.
    ///
    /// `worker_id` and `dp_rank` mirror Dynamo's `WorkerId` (`u64`) and
    /// `DpRank` (`u32`) exactly, so no conversion is needed at the fleet
    /// boundary.
    Local {
        worker_id: u64,
        dp_rank: u32,
        model: String,
    },
    /// A hosted model behind a provider API.
    Frontier { provider: String, model: String },
}

impl Target {
    pub fn model(&self) -> &str {
        match self {
            Target::Local { model, .. } | Target::Frontier { model, .. } => model,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Target::Local { .. })
    }

    /// How a [`TargetFilter`](crate::control::TargetFilter) names this target:
    /// `provider/model` for a hosted model, `local/model` for one of our own
    /// workers.
    ///
    /// Deliberately not [`Self::ledger_key`], and the difference is the point.
    /// A ledger key identifies a *cache*, so it names the worker; a policy
    /// identity identifies a *capability*, and a filter that named a worker
    /// would admit a turn on Monday and refuse the identical turn on Tuesday
    /// because the fleet scheduled it elsewhere. `dp_rank` is absent for the
    /// same reason it is absent there.
    ///
    /// One spelling for both halves of the fleet, so `local/*` and
    /// `anthropic/*` are sentences in the same language and an operator does
    /// not have to learn which side of the router they are configuring.
    pub fn policy_identity(&self) -> String {
        match self {
            Target::Local {
                model,
                worker_id: _,
                dp_rank: _,
            } => format!("local/{model}"),
            Target::Frontier { provider, model } => format!("{provider}/{model}"),
        }
    }

    /// Stable key for ledger lookups.
    ///
    /// Deliberately excludes `dp_rank`: cache residency is a property of the
    /// worker, and we want a turn that previously landed on a given worker to
    /// be recognized as warm regardless of rank.
    pub fn ledger_key(&self) -> String {
        match self {
            Target::Local {
                worker_id, model, ..
            } => format!("local:{model}:{worker_id}"),
            Target::Frontier { provider, model } => format!("frontier:{provider}:{model}"),
        }
    }
}

/// One option, priced.
///
/// Every field is an *expectation* — the point is that local and frontier
/// candidates are filled in by completely different machinery yet end up
/// directly comparable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub target: Target,

    /// Cache-adjusted prefill tokens. Locally this is the scheduler's
    /// `effective_prefill_tokens`; for frontier targets it is
    /// `isl - p_hit * cached_prefix_tokens`.
    pub expected_prefill_tokens: f64,

    /// Raw matched prefix tokens, before cache weighting. Observability only —
    /// decisions use `expected_prefill_tokens`.
    pub matched_prefix_tokens: u64,

    pub expected_ttft_ms: f64,
    pub expected_cost_usd: f64,

    /// Relative capability, 0.0..=1.0. Supplied by configuration, not measured.
    pub quality_prior: f64,

    /// Potential prefill tokens already booked on the worker: the same currency
    /// as `expected_prefill_tokens`, which is the point — pressure and cost sit
    /// on one axis instead of needing a conversion between queue depth and
    /// tokens. Absolute, not a fraction of capacity: expressing it as a
    /// utilization ratio would need a capacity denominator the fleet does not
    /// report.
    ///
    /// `None` for frontier targets, whose load is not observable to us.
    pub load: Option<f64>,
}

impl Candidate {
    /// Fraction of the prompt expected to be served from cache.
    pub fn cache_hit_ratio(&self, isl_tokens: usize) -> f64 {
        if isl_tokens == 0 {
            return 0.0;
        }
        let saved = (isl_tokens as f64 - self.expected_prefill_tokens).max(0.0);
        (saved / isl_tokens as f64).clamp(0.0, 1.0)
    }
}

/// Inputs available to a policy for one turn.
pub struct RoutingContext<'a> {
    pub session_id: &'a SessionId,
    /// 0 for the first turn of a session.
    pub turn_index: u64,
    pub isl_tokens: usize,
    pub candidates: &'a [Candidate],
    pub ledger: &'a CacheLedger,
    /// What this turn's principal is allowed to do, resolved once at admission
    /// and immutable for the turn.
    ///
    /// Arrives as *data* rather than as a second `Arc<dyn RoutingPolicy>`: the
    /// engine keeps exactly one policy object, and tenancy is a constraint on
    /// what that policy may choose rather than a different way of choosing.
    /// Every implementation consults it through [`Self::admissible`], which is
    /// where it is conjoined with the budget, and none re-derives it.
    pub turn_policy: &'a TurnPolicy,
    /// The session's frontier dispatches per routed turn, which is what
    /// [`FrontierCadence`](crate::control::FrontierCadence) is evaluated
    /// against. A projection of the log, borrowed from the session state.
    pub frontier_history: &'a FrontierHistory,
    /// What the spend ledger granted this turn.
    ///
    /// Turn-resolved data like [`Self::frontier_history`] and deliberately not
    /// admission-resolved like [`Self::turn_policy`]: a policy is fixed for the
    /// session while a grant is opened between `quote` and `choose` on every
    /// single turn and is stale by the next one. An unconfigured deployment
    /// passes [`TurnBudget::Unlimited`], which is the value that makes the
    /// budget axis a no-op rather than a ceiling that happens to be large.
    pub budget: &'a TurnBudget,
}

/// What the overflow valve appends to a rationale when it opens.
///
/// One string, because the fact is one fact however many policies can reach it,
/// and because an operator grepping the audit trail for overspend should find
/// every instance with one pattern.
const OVERFLOW_NOTE: &str = "; budget exhausted and no local candidate could take the turn, so the frontier pool was re-admitted";

/// The candidates one turn may be dispatched to, and the budget fact its
/// decision has to record.
///
/// Never empty: an empty admissible set is a routing *failure* with a blame
/// attached, so it leaves [`RoutingContext::admissible`] as an error rather
/// than as an empty pool a caller has to remember to check.
pub struct Admitted<'a> {
    pool: Vec<&'a Candidate>,
    budget_state: BudgetState,
}

impl<'a> Admitted<'a> {
    pub fn pool(&self) -> &[&'a Candidate] {
        &self.pool
    }

    /// Mint the decision for `target`, with `rationale` as the policy's own
    /// account of why.
    ///
    /// **The one place a [`Decision`]'s three coupled fields are filled in
    /// together.** Two of the three are this type's to answer for: the budget
    /// state — including [`BudgetState::ExhaustedOverflow`], which is
    /// *produced* by the admissibility resolution and exists nowhere upstream
    /// of it — and the valve's justification appended to the rationale. Neither
    /// is reachable any other way: the state is a private field and
    /// [`Self::annotate`] is a private method, so a third [`RoutingPolicy`]
    /// implementation cannot assemble a `Decision` by hand at all without
    /// visibly inventing a budget state — which is a different thing from
    /// quietly omitting one, and the difference is the whole of what used to be
    /// a paragraph on [`Decision::budget_state`] asking to be obeyed.
    ///
    /// `target` is not checked against [`Self::pool`]. A policy that returned
    /// something it was never offered is a bug this type cannot see — the pool
    /// holds borrows into the caller's candidate slice — and the engine's own
    /// `UnresolvableTarget` is where that is already caught, against the
    /// authoritative set.
    pub fn decide(&self, target: Target, rationale: String) -> Decision {
        Decision {
            target,
            rationale: self.annotate(rationale),
            budget_state: self.budget_state,
        }
    }

    /// The admissible candidate with the highest quality prior.
    ///
    /// The escalation audit branch's question, answered here rather than there
    /// because the pool's non-emptiness is this type's invariant: asked
    /// outside, it comes back as an `Option` whose `None` arm is unreachable
    /// and has to be given a wrong-but-plausible error anyway.
    pub fn highest_quality(&self) -> &'a Candidate {
        self.pool
            .iter()
            .copied()
            .reduce(|best, candidate| {
                if candidate.quality_prior > best.quality_prior {
                    candidate
                } else {
                    best
                }
            })
            .unwrap_or_else(|| unreachable!("an `Admitted` pool is never empty"))
    }

    /// `rationale`, with the valve's justification appended if it opened.
    ///
    /// Appended rather than woven in, so an ordinary decision's rationale is
    /// byte-identical to the one a pre-budget deployment wrote — the M1
    /// compatibility pin depends on exactly that.
    ///
    /// Private, and that is [`Self::decide`]'s doing: a policy that could take
    /// the annotated rationale without the budget state that explains it could
    /// still write half a decision.
    fn annotate(&self, rationale: String) -> String {
        match self.budget_state.overflowed() {
            true => rationale + OVERFLOW_NOTE,
            false => rationale,
        }
    }
}

impl<'a> RoutingContext<'a> {
    pub fn candidate_for(&self, target: &Target) -> Option<&Candidate> {
        self.candidates.iter().find(|c| &c.target == target)
    }

    /// The policy axes of admissibility, with the budget lifted — **the
    /// overflow valve's question, and the first filter
    /// [`Self::admissible`] applies.**
    ///
    /// Named rather than spelled as a full admissibility check with a
    /// fabricated [`TurnBudget::Unlimited`] handed in. A synthesized budget
    /// would be a second answer to "what did the ledger grant this turn", and
    /// the M2 review blocked on exactly that shape — a fabricated argument
    /// standing in for a question nobody had named. The valve relaxes precisely
    /// one axis, and the name is what says which.
    fn admits_past_the_budget(&self, candidate: &Candidate) -> bool {
        self.turn_policy.admits(candidate, self.frontier_history)
    }

    /// **The one admissibility question the router asks**: which candidates may
    /// this turn be dispatched to, and — when there are none — whose decision
    /// emptied the set.
    ///
    /// Four axes, two owners, and the split is the same one M2 named. The allow
    /// filter and the quality floor are *reachability* — the same answer on
    /// every turn of every session — and belong to
    /// [`TurnPolicy::permits`](crate::control::TurnPolicy::permits). The
    /// cadence and the budget are *this-turn* axes: a rationed model is
    /// reachable next turn and a budget-excluded one is reachable next month,
    /// which is why neither belongs in `permits` and why a candidate excluded
    /// by either still belongs in `considered` with its counterfactual saving
    /// intact. The first three are the policy's, which is why they arrive
    /// together through [`Self::admits_past_the_budget`]; the fourth is the
    /// ledger's.
    ///
    /// The conjunction lives here rather than inside `TurnPolicy` because the
    /// budget is not a property of a policy — a policy is resolved once at
    /// admission and a grant is opened between `quote` and `choose` on every
    /// turn. Threading a `TurnBudget` into
    /// [`TurnPolicy::admits`](crate::control::TurnPolicy::admits) would make one
    /// type answer for two clocks; this context is the thing that already holds
    /// both.
    ///
    /// **One piece of code decides all three outcomes**, because they are three
    /// answers to one question and a second implementation of any of them would
    /// blame a different system for the same fleet. `max_load` is the calling
    /// policy's own tuning: `None` means "do not exclude on load", which is what
    /// the escalation audit branch passes, deliberately, since an audit is worth
    /// reaching a busy worker for.
    ///
    /// **The order of the filters is the blame.** The turn policy runs first, so
    /// an empty set at that point is a refusal *this deployment made about this
    /// tenant* — [`RoutingError::PolicyRefused`], which a retry cannot fix and
    /// only an operator widening a policy can. What survives it is filtered by
    /// the budget and then by load, and an empty set at *that* point is a busy
    /// fleet or a spent budget rather than a refused tenant.
    ///
    /// **The valve is the last step and it relaxes one axis.** When the budget
    /// is exhausted, the project asked for the valve, and nothing survived — the
    /// local pool was load-rejected, or there was no local candidate to begin
    /// with — the *policy*-admitted candidates come back, budget aside. Load
    /// still applies to them (frontier candidates report none, so in practice
    /// it is the frontier pool that returns), and the allow filter, the quality
    /// floor and the cadence all still bind, because they were applied before
    /// the budget was and the valve never revisits them. The result is marked
    /// [`BudgetState::ExhaustedOverflow`], which is the only place that variant
    /// is produced.
    pub fn admissible(&self, max_load: Option<f64>) -> Result<Admitted<'a>, RoutingError> {
        let entitled: Vec<&'a Candidate> = self
            .candidates
            .iter()
            .filter(|candidate| self.admits_past_the_budget(candidate))
            .collect();
        if entitled.is_empty() {
            return Err(RoutingError::PolicyRefused);
        }

        let under_load = |candidate: &&Candidate| match (max_load, candidate.load) {
            (Some(ceiling), Some(load)) => load <= ceiling,
            _ => true,
        };
        let viable: Vec<&'a Candidate> = entitled
            .iter()
            .copied()
            .filter(|candidate| self.budget.admits(candidate))
            .filter(under_load)
            .collect();
        if !viable.is_empty() {
            return Ok(Admitted {
                pool: viable,
                budget_state: self.budget.state(),
            });
        }

        if self.budget.overflow_armed() {
            let overflowed: Vec<&'a Candidate> = entitled.into_iter().filter(under_load).collect();
            if !overflowed.is_empty() {
                return Ok(Admitted {
                    pool: overflowed,
                    budget_state: BudgetState::ExhaustedOverflow,
                });
            }
        }

        Err(RoutingError::NoViableCandidate {
            budget_state: self.budget.state(),
        })
    }
}

/// A policy's choice, with the reasoning preserved for the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub target: Target,
    pub rationale: String,
    /// The budget situation this choice was made under.
    ///
    /// Carried out of `choose` rather than read back off the
    /// [`RoutingContext`] by the caller, because one of its four values is
    /// *produced* here: only the admissibility resolution knows whether the
    /// overflow valve had to open, and a caller re-deriving the state from the
    /// grant it handed in would record every overflow as an ordinary
    /// exhausted turn.
    ///
    /// It is filled in by [`Admitted::decide`] and there is nowhere else to
    /// get it from — the readers that used to hand it out are private to this
    /// module — which is what turned "remember to carry this" from a paragraph
    /// every future [`RoutingPolicy`] had to read into a thing the module
    /// boundary decides.
    pub budget_state: BudgetState,
}

/// The persisted form of a decision, written into the session event log.
///
/// Carries the losing options as well as the winner: without them the ledger
/// cannot answer "was that the right call?" after the fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub chosen: Target,
    pub rationale: String,
    pub policy: String,
    pub isl_tokens: u64,
    pub expected_prefill_tokens: f64,
    pub expected_cost_usd: f64,
    pub considered: Vec<Candidate>,
    /// Fingerprint of the [`TurnPolicy`] in force when this decision was made.
    ///
    /// The audit trail's answer to "under what constraints was this chosen?",
    /// recorded on the decision itself so a policy change is visible on the
    /// very next routing event with no side channel able to disagree with it.
    ///
    /// The empty string means a log written before per-principal policy
    /// existed — the serde default, the same treatment
    /// [`Usage::reasoning_tokens`](crate::event::Usage::reasoning_tokens)
    /// gets, and for the same reason: history has to keep deserializing after
    /// the type grows. It is not a policy that happens to fingerprint to
    /// nothing; [`TurnPolicy::unrestricted`] has a real digest.
    #[serde(default)]
    pub turn_policy_digest: String,
    /// The budget situation in force when this decision was made.
    ///
    /// Recorded because a project that stayed under budget by serving four
    /// hundred turns on a 7B model has not had the same month as one that never
    /// needed to, and because
    /// [`ExhaustedOverflow`](BudgetState::ExhaustedOverflow) is the dashboard
    /// number for "served on frontier past exhaustion because local was
    /// saturated" — a fact that exists nowhere else in the log.
    ///
    /// Defaults to [`Unconstrained`](BudgetState::Unconstrained), which is the
    /// correct reading of a log written before budgets existed rather than a
    /// placeholder: those turns really were taken under no budget. Same
    /// treatment, and same reason, as [`Self::turn_policy_digest`] above.
    #[serde(default)]
    pub budget_state: BudgetState,
    /// The rate card in force when this decision was made.
    ///
    /// A fact about the turn, exactly like the two fields above it, and
    /// recorded for the same reason: what the spend ledger charges has to be
    /// derivable from the log alone. A settle is driven twice — once by the
    /// process that ran the turn and, if that process died first, once by the
    /// successor that replays its log — and pricing either of them against the
    /// *live* catalog makes the two answers depend on which file the process
    /// happened to boot with. A price list is a file an operator edits, so
    /// that is not a hypothetical: dropping a model is an ordinary edit, and
    /// against a live catalog it turns every later settle of a turn that used
    /// it into an error.
    ///
    /// `None` for a local dispatch, which bills capacity and not dollars, and
    /// for the pre-M3 logs the serde default covers. A frontier decision
    /// carrying `None` is therefore exactly one thing: a turn from before this
    /// field existed, whose settle can no longer be priced from the log. That
    /// is drift a repair reports and skips — see the settle seam in
    /// `roundhouse-server`'s engine — rather than a turn anyone can fix now.
    #[serde(default)]
    pub rate_card: Option<ProviderPricing>,
    /// Whose credential this dispatch spends.
    ///
    /// Decided where the credential resolves — before `choose()`, at the same
    /// seam the candidate set is filtered — because a fact decided there and
    /// read again at settle time has to travel in the log, or the process that
    /// ran the turn and the successor that replays it can reach two different
    /// answers about who was billed.
    ///
    /// Defaults to [`Payer::Deployment`], which is the correct reading of a
    /// pre-M7 log rather than a placeholder: those turns really were paid for
    /// with the deployment's own key. Same treatment, and same reason, as
    /// [`Self::budget_state`].
    #[serde(default)]
    pub payer: Payer,
    /// Providers quoted for this turn and dropped for want of a credential.
    ///
    /// **The marker on a credential degrade**, and the reason it is recorded
    /// rather than inferred: a project whose credential variable was never set
    /// serves every turn locally and looks, in every other field of this
    /// record, exactly like a project that simply prefers its own workers. That
    /// is the silent failure this milestone's auth ruling found on the client
    /// side, and it is not one worth reproducing on ours.
    ///
    /// Empty on every ordinary turn and skipped on the wire when it is, so a
    /// deployment that has never configured a credential writes the same
    /// decision bytes it wrote before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld_providers: Vec<String>,
}

/// Why no target was chosen.
///
/// The three empty-set arms are told apart by *whose decision emptied the
/// set*, because that is the only thing a reader of the log can act on. They
/// are three because the answers differ: `NoCandidates` sends an operator to
/// the fleet and the catalog, `PolicyRefused` sends them to the control-plane
/// file, and `NoViableCandidate` sends them to the deployment's own routing
/// tuning or to the workers it excluded. Collapsing any two of them means one
/// of those readers is sent to the wrong system.
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// Nothing was quoted at all.
    #[error("no candidates available for routing")]
    NoCandidates,
    /// Candidates were quoted, and this turn's [`TurnPolicy`] admitted none of
    /// them.
    ///
    /// A decision this deployment made about this tenant, and the one terminal
    /// outcome a retry cannot change: the same turn under the same policy
    /// refuses again, and only an operator widening the policy moves it. It
    /// reaches the log as
    /// [`IncompleteReason::PolicyRefused`](crate::event::IncompleteReason::PolicyRefused).
    #[error("no target this turn's policy admits was available")]
    PolicyRefused,
    /// Candidates were quoted, the turn's policy admitted some, and the
    /// deployment's own routing constraints excluded the rest.
    ///
    /// A busy fleet, not a refused tenant. Reporting it as a policy refusal
    /// would tell a client that widening a policy is the fix for an overloaded
    /// worker, and send an operator to read a `TurnPolicy` that is not the
    /// problem.
    ///
    /// It carries the budget state because the two facts arrive together in the
    /// one case an operator most needs both of: a project whose budget is spent
    /// has had every frontier candidate excluded before load was ever
    /// considered, so the local pool emptying under load is the *whole* of the
    /// remaining fleet emptying. Blaming the fleet is still right — the pool
    /// was emptied by load, not by the tenant's policy — but an operator told
    /// only that goes tuning workers without noticing there was nothing to fall
    /// back to. Every other state contributes nothing to the message, so the
    /// ordinary busy-fleet error reads exactly as it did before budgets
    /// existed.
    #[error(
        "no candidate satisfied the routing policy's own constraints{}",
        .budget_state.saturation_note()
    )]
    NoViableCandidate { budget_state: BudgetState },
    #[error("policy failure: {0}")]
    Policy(#[from] anyhow::Error),
}

/// Chooses a target for one turn.
///
/// Implementations must be pure with respect to the context: the session layer
/// records the returned [`Decision`] before any execution begins, so a policy
/// that mutated shared state here would produce an audit trail that disagrees
/// with what actually happened.
#[async_trait]
pub trait RoutingPolicy: Send + Sync {
    /// Stable name, recorded in [`DecisionRecord::policy`].
    fn name(&self) -> &str;

    async fn choose(&self, ctx: &RoutingContext<'_>) -> Result<Decision, RoutingError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_ratio_reflects_prefill_savings() {
        let candidate = Candidate {
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            expected_prefill_tokens: 250.0,
            matched_prefix_tokens: 750,
            expected_ttft_ms: 400.0,
            expected_cost_usd: 0.01,
            quality_prior: 0.95,
            load: None,
        };
        assert!((candidate.cache_hit_ratio(1000) - 0.75).abs() < 1e-9);
        assert_eq!(candidate.cache_hit_ratio(0), 0.0);
    }

    #[test]
    fn ledger_key_ignores_dp_rank_but_separates_workers() {
        let a = Target::Local {
            worker_id: 1,
            dp_rank: 0,
            model: "llama".into(),
        };
        let b = Target::Local {
            worker_id: 1,
            dp_rank: 3,
            model: "llama".into(),
        };
        let c = Target::Local {
            worker_id: 2,
            dp_rank: 0,
            model: "llama".into(),
        };
        assert_eq!(a.ledger_key(), b.ledger_key());
        assert_ne!(a.ledger_key(), c.ledger_key());
    }

    #[test]
    fn a_policy_identity_names_a_capability_and_a_ledger_key_names_a_cache() {
        let worker_a = Target::Local {
            worker_id: 1,
            dp_rank: 0,
            model: "llama".into(),
        };
        let worker_b = Target::Local {
            worker_id: 2,
            dp_rank: 3,
            model: "llama".into(),
        };
        assert_eq!(worker_a.policy_identity(), "local/llama");
        assert_eq!(
            worker_a.policy_identity(),
            worker_b.policy_identity(),
            "which worker served the turn is not a policy question"
        );
        assert_ne!(
            worker_a.ledger_key(),
            worker_b.ledger_key(),
            "the control: it is very much a cache question"
        );
        assert_eq!(
            Target::Frontier {
                provider: "anthropic".into(),
                model: "claude-opus-4".into(),
            }
            .policy_identity(),
            "anthropic/claude-opus-4"
        );
    }

    #[test]
    fn a_pre_m2_decision_record_reads_back_with_an_empty_policy_digest() {
        // Byte-for-byte what a `Routed` decision serialized to before
        // per-principal policy existed. Logs in this shape are still replayed
        // after an upgrade, and a fold that refused to parse them would take
        // the deployment's whole routing history with it.
        let json = r#"{
            "chosen": {"kind":"frontier","provider":"anthropic","model":"claude"},
            "rationale": "test",
            "policy": "affinity",
            "isl_tokens": 4096,
            "expected_prefill_tokens": 4096.0,
            "expected_cost_usd": 0.02,
            "considered": []
        }"#;
        let record: DecisionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(
            record.turn_policy_digest, "",
            "an absent digest is `pre-M2`, which is a fact and not a policy"
        );

        // And a record written today round-trips its digest, or replaying a
        // log would report constraints that were never in force.
        let digested = DecisionRecord {
            turn_policy_digest: "0123456789abcdef".into(),
            ..record
        };
        let round_tripped: DecisionRecord =
            serde_json::from_str(&serde_json::to_string(&digested).unwrap()).unwrap();
        assert_eq!(round_tripped, digested);
    }

    #[test]
    fn an_overflow_dispatch_is_a_marked_fact_and_a_pre_m3_log_reads_unconstrained() {
        // Both directions, because the field has to survive both. An overflow
        // that did not round-trip would lose the one number that answers "how
        // much did this project spend past its limit because its own fleet was
        // full" — and a record written before budgets existed has to keep
        // deserializing, or an upgrade takes the deployment's routing history
        // with it.
        let record = DecisionRecord {
            chosen: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            rationale: "overflow".into(),
            policy: "affinity".into(),
            isl_tokens: 4_096,
            expected_prefill_tokens: 4_096.0,
            expected_cost_usd: 0.02,
            considered: Vec::new(),
            turn_policy_digest: "0123456789abcdef".into(),
            budget_state: BudgetState::ExhaustedOverflow,
            rate_card: Some(ProviderPricing {
                input_per_mtok_usd: 3.0,
                cached_input_per_mtok_usd: 0.3,
                cache_write_per_mtok_usd: 3.75,
                output_per_mtok_usd: 15.0,
            }),
            payer: Payer::User,
            withheld_providers: vec!["openai".into()],
        };
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(
            encoded.contains(r#""budget_state":"exhausted_overflow""#),
            "the overspend has to be findable in the log by one grep: {encoded}"
        );
        assert!(
            encoded.contains(r#""payer":"user""#),
            "and so does whose credential paid for it: {encoded}"
        );
        assert_eq!(
            serde_json::from_str::<DecisionRecord>(&encoded).unwrap(),
            record,
            "the card has to survive the round trip too, or a settle re-driven \
             from this log prices the turn at nothing"
        );

        // Byte-for-byte what a `Routed` decision serialized to at M2 — with a
        // policy digest, because that field already existed, and without a
        // budget state, because this one did not.
        let pre_m3 = r#"{
            "chosen": {"kind":"local","worker_id":7,"dp_rank":0,"model":"llama"},
            "rationale": "test",
            "policy": "affinity",
            "isl_tokens": 4096,
            "expected_prefill_tokens": 512.0,
            "expected_cost_usd": 0.0,
            "considered": [],
            "turn_policy_digest": "4ec325a715649c8e"
        }"#;
        let recovered: DecisionRecord = serde_json::from_str(pre_m3).unwrap();
        assert_eq!(
            recovered.budget_state,
            BudgetState::Unconstrained,
            "a turn taken before budgets existed was taken under no budget, \
             which is a fact and not a missing value"
        );
        assert!(
            !recovered.budget_state.overflowed(),
            "and it certainly did not overflow one"
        );
        assert_eq!(
            recovered.rate_card, None,
            "and it recorded no rate card, because there was no field to record \
             one in -- which is why a repair reports such a settle as drift \
             rather than pricing it at zero"
        );
        assert_eq!(
            recovered.payer,
            Payer::Deployment,
            "a turn taken before BYOK existed really was paid for with the \
             deployment's own key, which is a fact and not a missing value"
        );
        assert!(
            recovered.withheld_providers.is_empty(),
            "and nothing was withheld from it, because there was nothing to \
             withhold on"
        );

        // The credential marker is skipped when empty, so a deployment that
        // never configures one keeps writing exactly the bytes it wrote before
        // this field existed.
        let ordinary = DecisionRecord {
            payer: Payer::Deployment,
            withheld_providers: Vec::new(),
            ..record
        };
        let encoded = serde_json::to_string(&ordinary).unwrap();
        assert!(
            !encoded.contains("withheld_providers"),
            "an empty marker is absent from the wire, not present and empty: {encoded}"
        );
    }
}
