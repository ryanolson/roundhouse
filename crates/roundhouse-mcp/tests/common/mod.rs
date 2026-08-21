// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A deployment, faked at the seam and nowhere else.
//!
//! [`FakeDeployment`] implements [`ControlReads`] and is the only stand-in in
//! these tests: the store, the overlay arithmetic, the dispatcher and the
//! surface are all the real ones. That is the point of the seam — the surface
//! is exercised against the same trait `roundhouse-server` implements, so a
//! test that passes here fails there only if the server's implementation
//! disagrees with the trait's documented contract.
//!
//! **The fake owns the prices.** It answers
//! [`ControlReads::admissible_targets`] by building real
//! [`Candidate`]s and asking [`TurnPolicy::permits`], exactly as the server
//! does. That is what keeps the crate under test from ever fabricating a
//! candidate to ask an admissibility question with — and it is what lets the
//! no-prices assertions be about numbers a real catalog would have carried.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use roundhouse_core::control::{
    Balance, BudgetState, LedgerState, Principal, TargetFilter, TurnPolicy,
};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::{Candidate, DecisionRecord, Target};
use roundhouse_mcp::reads::{ControlReads, SessionFacts};
use roundhouse_mcp::surface::SurfaceError;
use roundhouse_mcp::{ControlPlaneSurface, ControlStore};

/// A distinctive per-model price, so a rendering that leaked one is visible as
/// a literal rather than as a plausible number.
pub const CLAUDE_PRICE_USD: f64 = 7.77;
pub const GPT_PRICE_USD: f64 = 6.66;
pub const LOCAL_PRICE_USD: f64 = 0.0;

pub fn local(model: &str) -> Target {
    Target::Local {
        worker_id: 1,
        dp_rank: 0,
        model: model.into(),
    }
}

pub fn frontier(provider: &str, model: &str) -> Target {
    Target::Frontier {
        provider: provider.into(),
        model: model.into(),
    }
}

pub fn ada() -> Principal {
    Principal::new("acme", "ada")
}

pub fn bob() -> Principal {
    Principal::new("other", "bob")
}

pub fn adas_session() -> SessionId {
    SessionId::new("acme/ada/sess_1")
}

/// One catalog row: what a target is, how good it is, what it costs, and how
/// warm it is.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub target: Target,
    pub quality_prior: f64,
    pub price_usd: f64,
    /// Cache-adjusted prefill, the axis [`AffinityPolicy`] weights highest.
    ///
    /// Here rather than fixed at one value for every row because [`decision`]
    /// runs the *real* router over these candidates, and a fleet where every row
    /// is equally warm makes cost the only tiebreak — which hands every turn to
    /// the free local worker and produces a rationale carrying `$0.00000`. A
    /// no-prices assertion against that string would pass for a policy that
    /// prints prices, which is the tautology this fixture exists to avoid.
    pub prefill_tokens: f64,
}

impl CatalogRow {
    fn candidate(&self) -> Candidate {
        Candidate {
            target: self.target.clone(),
            expected_prefill_tokens: self.prefill_tokens,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: self.price_usd,
            quality_prior: self.quality_prior,
            load: None,
        }
    }
}

/// The mixed fleet most tests run against.
///
/// The hosted Anthropic row is warm and the other two are cold, which is what
/// makes the real router pick a *priced* target — see
/// [`CatalogRow::prefill_tokens`].
pub fn mixed_fleet() -> Vec<CatalogRow> {
    vec![
        CatalogRow {
            target: local("llama-3.1-8b"),
            quality_prior: 0.6,
            price_usd: LOCAL_PRICE_USD,
            prefill_tokens: 1_000.0,
        },
        CatalogRow {
            target: frontier("anthropic", "claude-opus-4"),
            quality_prior: 0.95,
            price_usd: CLAUDE_PRICE_USD,
            prefill_tokens: 200.0,
        },
        CatalogRow {
            target: frontier("openai", "gpt-5"),
            quality_prior: 0.9,
            price_usd: GPT_PRICE_USD,
            prefill_tokens: 1_000.0,
        },
    ]
}

/// A deployment with local capacity and nothing else.
pub fn local_only_fleet() -> Vec<CatalogRow> {
    vec![CatalogRow {
        target: local("llama-3.1-8b"),
        quality_prior: 0.6,
        price_usd: LOCAL_PRICE_USD,
        prefill_tokens: 1_000.0,
    }]
}

pub struct FakeDeployment {
    pub ceiling: TurnPolicy,
    pub catalog: Vec<CatalogRow>,
    /// `None` for a deployment that meters nothing, which is what open mode and
    /// a project with no `"budget"` both are.
    pub balance: Option<Balance>,
    /// The most recent session per principal.
    pub sessions: HashMap<Principal, SessionId>,
    pub facts: HashMap<SessionId, SessionFacts>,
    pub now_ms: u64,
}

impl Default for FakeDeployment {
    fn default() -> Self {
        let mut sessions = HashMap::new();
        sessions.insert(ada(), adas_session());
        Self {
            ceiling: TurnPolicy::unrestricted(),
            catalog: mixed_fleet(),
            balance: Some(Balance {
                committed_usd: 12.0,
                held_usd: 0.0,
                project_remaining_usd: 88.0,
                member_committed_usd: 3.0,
                member_remaining_usd: Some(17.0),
                state: LedgerState::Unconstrained,
            }),
            sessions,
            facts: HashMap::new(),
            now_ms: 1_700_000_000_000,
        }
    }
}

impl FakeDeployment {
    /// A deployment whose configured policy admits local models only — the
    /// ceiling the plan's `prefer frontier` example runs against.
    pub fn local_only() -> Self {
        Self {
            ceiling: TurnPolicy {
                allow: TargetFilter::parse(["local/*"]).unwrap(),
                ..TurnPolicy::unrestricted()
            },
            catalog: mixed_fleet(),
            ..Self::default()
        }
    }

    pub fn with_facts(mut self, session: &SessionId, facts: SessionFacts) -> Self {
        self.facts.insert(session.clone(), facts);
        self
    }

    /// The surface under test, with a store the caller can also reach.
    pub fn surface(self) -> (ControlPlaneSurface<FakeDeployment>, Arc<ControlStore>) {
        let store = Arc::new(ControlStore::new());
        (
            ControlPlaneSurface::new(Arc::new(self), Arc::clone(&store)),
            store,
        )
    }
}

#[async_trait]
impl ControlReads for FakeDeployment {
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
    ) -> Result<SessionId, SurfaceError> {
        match conversation {
            None => self
                .sessions
                .get(principal)
                .cloned()
                .ok_or(SurfaceError::NoSession),
            Some(named) => {
                // The server resolves this through the same `bound_session`
                // namespacing the Responses surface uses; the fake reproduces
                // the one property the surface depends on, which is that a
                // conversation outside the caller's namespace is refused rather
                // than silently replaced by the caller's own.
                let qualified = format!("{}{named}", principal.namespace_prefix());
                if self.sessions.values().any(|id| id.as_str() == qualified) {
                    Ok(SessionId::new(qualified))
                } else {
                    Err(SurfaceError::ForeignConversation(named.to_string()))
                }
            }
        }
    }

    async fn ceiling_policy(&self, _principal: &Principal) -> Result<TurnPolicy, SurfaceError> {
        Ok(self.ceiling.clone())
    }

    async fn admissible_targets(
        &self,
        _principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError> {
        Ok(self
            .catalog
            .iter()
            .map(CatalogRow::candidate)
            .filter(|candidate| policy.permits(candidate))
            .map(|candidate| candidate.target)
            .collect())
    }

    async fn balance(&self, _principal: &Principal) -> Result<Option<Balance>, SurfaceError> {
        Ok(self.balance)
    }

    async fn session_facts(&self, session: &SessionId) -> Result<SessionFacts, SurfaceError> {
        Ok(self.facts.get(session).cloned().unwrap_or_default())
    }

    fn now_ms(&self) -> u64 {
        self.now_ms
    }
}

/// Counts calls into each [`ControlReads`] method, delegating every one to
/// `inner`.
///
/// `fetch_steer`'s module doc claims it does "no clock, no fleet, no judge" —
/// a pure read of a payload committed at emit time, with no extra call into
/// the deployment. Nothing enforced that claim: a handler that quietly added
/// a `ceiling_policy` or `admissible_targets` call before the steer lookup
/// changed no test's output. This wrapper is what lets a test assert the
/// count directly rather than trust the prose.
pub struct CountingReads<R> {
    inner: R,
    pub resolve_session_calls: AtomicUsize,
    pub ceiling_policy_calls: AtomicUsize,
    pub admissible_targets_calls: AtomicUsize,
    pub balance_calls: AtomicUsize,
    pub session_facts_calls: AtomicUsize,
    pub session_cursor_calls: AtomicUsize,
}

impl<R> CountingReads<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            resolve_session_calls: AtomicUsize::new(0),
            ceiling_policy_calls: AtomicUsize::new(0),
            admissible_targets_calls: AtomicUsize::new(0),
            balance_calls: AtomicUsize::new(0),
            session_facts_calls: AtomicUsize::new(0),
            session_cursor_calls: AtomicUsize::new(0),
        }
    }

    /// Every call across every method, added together — the number a "does no
    /// extra read work" assertion actually wants, so a test does not have to
    /// enumerate methods and risk missing one a future change adds.
    pub fn total_calls(&self) -> usize {
        self.resolve_session_calls.load(Ordering::SeqCst)
            + self.ceiling_policy_calls.load(Ordering::SeqCst)
            + self.admissible_targets_calls.load(Ordering::SeqCst)
            + self.balance_calls.load(Ordering::SeqCst)
            + self.session_facts_calls.load(Ordering::SeqCst)
            + self.session_cursor_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl<R: ControlReads> ControlReads for CountingReads<R> {
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
    ) -> Result<SessionId, SurfaceError> {
        self.resolve_session_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.resolve_session(principal, conversation).await
    }

    async fn ceiling_policy(&self, principal: &Principal) -> Result<TurnPolicy, SurfaceError> {
        self.ceiling_policy_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.ceiling_policy(principal).await
    }

    async fn admissible_targets(
        &self,
        principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError> {
        self.admissible_targets_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.admissible_targets(principal, policy).await
    }

    async fn balance(&self, principal: &Principal) -> Result<Option<Balance>, SurfaceError> {
        self.balance_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.balance(principal).await
    }

    async fn session_facts(&self, session: &SessionId) -> Result<SessionFacts, SurfaceError> {
        self.session_facts_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.session_facts(session).await
    }

    /// Forwarded rather than left to the trait's default, or the wrapper would
    /// silently answer "no cheap cursor" for an inner that has one and turn
    /// every memo assertion made through it into a study of the wrapper.
    async fn session_cursor(&self, session: &SessionId) -> Result<Option<u64>, SurfaceError> {
        self.session_cursor_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.session_cursor(session).await
    }

    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }
}

/// How many tokens the fixture turn's prompt is.
const FIXTURE_ISL_TOKENS: usize = 1_200;

/// A routing decision with every field `explain_last_route` renders — and the
/// two it must not — **as a real routing policy produced it**.
///
/// The rationale is not a literal. `AffinityPolicy` is the policy this
/// deployment wires (`main.rs`), `engine.rs` copies `Decision::rationale` into
/// `DecisionRecord::rationale` verbatim, and `plane.rs` copies that into
/// `RouteExplanation` verbatim — so a hand-written rationale here tests the two
/// copies and nothing else. It certainly cannot test the no-prices rule: a
/// literal chosen by the test author contains a price only if the author put one
/// there, which makes the assertion true by construction and blind to a
/// *producer* that formats one in. Running the real policy is what makes the
/// guard a statement about the string a deployment actually writes.
///
/// `budget_state` is the one field set by hand. The router answers
/// `Unconstrained` for an unmetered turn, and the rendering under test is the
/// mapping from the enum to its wire spelling — which a fixture stuck on the
/// default value would exercise for exactly one of four arms.
pub async fn decision() -> DecisionRecord {
    use roundhouse_core::control::{FrontierHistory, TurnBudget};
    use roundhouse_core::routing::{AffinityPolicy, CacheLedger, RoutingContext, RoutingPolicy};

    let considered: Vec<Candidate> = mixed_fleet().iter().map(CatalogRow::candidate).collect();
    let policy = AffinityPolicy::new();
    let ceiling = TurnPolicy::unrestricted();
    let ledger = CacheLedger::new();
    let history = FrontierHistory::default();
    let budget = TurnBudget::Unlimited;
    let chosen = policy
        .choose(&RoutingContext {
            session_id: &adas_session(),
            turn_index: 1,
            isl_tokens: FIXTURE_ISL_TOKENS,
            candidates: &considered,
            ledger: &ledger,
            turn_policy: &ceiling,
            frontier_history: &history,
            budget: &budget,
        })
        .await
        .expect("a three-candidate fleet under an unrestricted policy routes");
    let winner = considered
        .iter()
        .find(|candidate| candidate.target == chosen.target)
        .expect("the router chooses from the set it was handed");

    DecisionRecord {
        chosen: chosen.target.clone(),
        rationale: chosen.rationale,
        policy: policy.name().to_string(),
        isl_tokens: FIXTURE_ISL_TOKENS as u64,
        expected_prefill_tokens: winner.expected_prefill_tokens,
        expected_cost_usd: winner.expected_cost_usd,
        considered,
        turn_policy_digest: ceiling.digest(),
        budget_state: BudgetState::Warned,
        rate_card: None,
        payer: Default::default(),
        billing: Default::default(),
        budget_draw: None,
        withheld_providers: Vec::new(),
    }
}
