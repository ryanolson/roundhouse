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
//! [`RoutingPolicy`] is the seam that keeps Switchyard optional. Its
//! `Algorithm` trait is a good fit — the algorithm emits `Step::CallLlm` with a
//! semantic target and the *host* executes it — but `libsy::State` is in-memory
//! with no pluggable persistence, which collides with surviving process death.
//! Keeping libsy behind this trait means our session core never depends on it.

pub mod ledger;
pub mod policy;

pub use ledger::{CacheLedger, CacheModel, LedgerEntry, ProviderPricing};
pub use policy::{AffinityPolicy, EscalationPolicy};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::control::{FrontierHistory, TurnPolicy};
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
    /// Every implementation consults it through
    /// [`TurnPolicy::admits`](crate::control::TurnPolicy::admits) and none
    /// re-derives it.
    pub turn_policy: &'a TurnPolicy,
    /// The session's frontier dispatches per routed turn, which is what
    /// [`FrontierCadence`](crate::control::FrontierCadence) is evaluated
    /// against. A projection of the log, borrowed from the session state.
    pub frontier_history: &'a FrontierHistory,
}

impl RoutingContext<'_> {
    pub fn candidate_for(&self, target: &Target) -> Option<&Candidate> {
        self.candidates.iter().find(|c| &c.target == target)
    }
}

/// A policy's choice, with the reasoning preserved for the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub target: Target,
    pub rationale: String,
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
    #[error("no candidate satisfied the routing policy's own constraints")]
    NoViableCandidate,
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
}
