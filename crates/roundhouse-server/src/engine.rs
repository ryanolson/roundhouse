// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turn execution.
//!
//! Two halves live one module down, each because it answers to a different
//! store than the log this file is about.
//!
//! [`spend`] is the money: what a turn may reserve, what it is charged, and how
//! a settle lost to a crash is put right. It answers to a durable counter two
//! processes race for rather than to a model call under a deadline, and
//! everything in it has to be as true for a successor replaying a log as for the
//! process that wrote it.
//!
//! [`control`] is the agent's own half of the conversation: the overlay it asked
//! for, spent at the start of this turn, and the corrective payload a steered
//! turn deposits for it to fetch. It answers to node-local process state that
//! does not survive a restart — which is safe only because every write in it is
//! a *narrowing* or a projection of something the log already holds, and losing
//! either degrades to the deployment's own ceiling rather than past it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use roundhouse_core::context::{ContextAssembler, Tokenizer};
use roundhouse_core::control::{
    Billing, CredentialError, FairUseError, FairUseLedger, MemoryFairUseLedger, MemorySpendLedger,
    SpendError, SpendLedger, TurnCredential, TurnPolicy,
};
use roundhouse_core::event::{Accounting, IncompleteReason, SessionObserver, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, SideCallId, TurnId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::Item;
use roundhouse_core::metrics::MetricsRecorder;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{
    AttemptClass, CacheLedger, Candidate, Decision, DecisionRecord, DecisionSource,
    DispatchAttempt, RoutingContext, RoutingError, RoutingPolicy, Target, Tier, TierRecipe,
    TurnSignals,
};
use roundhouse_core::session::{Session, SessionError, SessionState, TurnAdmission};
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{SideCall, exchanges};
use roundhouse_fleet::{
    FleetError, FleetQuery, FrontierChunk, FrontierClient, FrontierClients, FrontierError,
    FrontierQuote, FrontierStream, LocalFleet, LocalQuote, StaticFrontierCatalog,
};
use roundhouse_mcp::ControlStore;
use tokio::time::Instant;

use crate::control_config::Admission;

mod control;
mod fair_use;
pub(crate) mod spend;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Routing(#[from] RoutingError),
    #[error(transparent)]
    Fleet(#[from] FleetError),
    #[error(transparent)]
    Frontier(#[from] FrontierError),
    #[error(transparent)]
    Spend(#[from] SpendError),
    /// The fair-use ledger could not answer.
    ///
    /// Its own arm rather than folded into [`Self::Spend`]: the two are
    /// different stores answering different questions, and an operator holding
    /// a failure has to know which counter is down — the durable one that
    /// decides whether money may be spent, or the rolling one that decides
    /// whether a session has had its share.
    #[error(transparent)]
    FairUse(#[from] FairUseError),
    /// The project's budget is spent and it is configured to refuse.
    ///
    /// A decision this deployment made about this tenant, like
    /// [`RoutingError::PolicyRefused`] — and unlike it in the one way that
    /// matters to whoever is holding the failure: an admin can raise a limit,
    /// and a monthly window lifts on its own.
    #[error("the project's budget is spent and it refuses rather than degrading to local")]
    BudgetRefused,
    /// A frontier dispatch whose decision recorded no rate card.
    ///
    /// Loud rather than free. Every other route to a zero settle is a
    /// statement — a local dispatch cost nothing, a turn that reached no
    /// provider owes nothing — and treating "we could not find the rate card"
    /// as a fourth one would book unpriced frontier traffic as a saving, which
    /// is the one accounting lie the whole ledger exists to make impossible.
    ///
    /// **Two readings, and only one of them is a bug.** Reached from a live
    /// settle it means this build routed to a hosted model and wrote a decision
    /// with no price on it, which cannot happen through `plan` and is a defect
    /// if it ever does. Reached from a repair it means the log predates the
    /// recorded card, which is not a defect and not fixable now: that turn's
    /// spend is lost to drift, and [`Engine::repair_settle`] logs it and moves
    /// on rather than failing a session over a turn nobody can price any more.
    /// It is emphatically *not* what it used to be — "the catalog no longer
    /// lists this model" — because a settle no longer asks a catalog anything.
    #[error("a frontier dispatch to `{0:?}` settled against a decision that recorded no rate card")]
    UnpricedSettlement(Target),
    #[error("chosen target `{0:?}` had no matching quote")]
    UnresolvableTarget(Target),
    #[error("turn exceeded its deadline of {0} ms")]
    TurnDeadline(u64),
}

/// A dispatch that never opened a stream, and whether another target is worth
/// trying.
///
/// The `Option` is the whole type: `Some` is an outage and `None` is an answer.
/// Keeping the classification beside the error — rather than re-deriving it at
/// the loop from an `EngineError` that has already flattened the variant — is
/// what stops "should this fail over" from becoming a question about a
/// formatted string.
struct ConnectFailure {
    error: EngineError,
    class: Option<AttemptClass>,
}

impl ConnectFailure {
    /// A failure no second target would survive: a refused credential, a
    /// dialect nobody can serialize, a 404, a struck deadline.
    fn terminal(error: impl Into<EngineError>) -> Self {
        Self {
            error: error.into(),
            class: None,
        }
    }
}

/// Did this turn move the session onto the capable tier, and did a *signal* say
/// so? (S6)
///
/// The tier half of the handoff gate, in one function because it is three
/// conditions that only mean anything together — and written against the
/// **targets in the recipe** rather than against a tier recorded on the
/// decision, which is the shape a first draft had.
///
/// **Why membership and not a `tier` field.** Recording the served tier on the
/// `Decision` and the `DecisionRecord` would answer the same question, at the
/// cost of a field on the persisted record and a construction site in fifteen
/// files — and it would put the load-bearing correctness on somebody remembering
/// to set it to the tier that *served* rather than the tier the scorer *picked*.
/// Those differ exactly when the picked tier is entirely inadmissible and
/// `StagePolicy` falls to the other one: the capable model is not answering, and
/// a note claiming the preceding steps are not to be trusted would be the
/// best-effort-narrowing lie `validate::handoff` argues against at length.
/// Asking "is the target that is about to be dispatched named in the capable
/// list" cannot get that wrong — `tier_pool` sourced it from one list or the
/// other, and `TierRecipe::new` refuses a target named in both, so membership is
/// unambiguous.
///
/// **Why `last_decision` and not the `ActiveEscalation::decided_on_turn` shape**
/// the validate loop's half uses. An escalation is a multi-turn narrowing with a
/// life, so "the turn it began on" is a fact worth folding; a tier is re-decided
/// from scratch every turn, so the only comparison that means anything is
/// against the turn immediately before. There is no state to fold and none is
/// added.
///
/// Three consequences, none of them clean, all of them cheaper than the
/// alternative:
///
/// - **A recipe edited mid-session degrades wrong in one direction.** A capable
///   target dropped from the list between turns reads as not-capable, so the
///   next capable turn narrates a second time and tells the model the preceding
///   steps came from something in trouble when they came from the same tier.
///   Reachable only through a live admin edit. The `tier`-field shape has the
///   mirror image — a recorded `Capable` from a recipe that no longer exists —
///   so neither spelling is clean and this one costs no persisted state.
/// - **A previous turn that exhausted its capable fallbacks and failed still
///   counts as capable**, because its last `Routed` names a capable target. No
///   note rides the turn after it. Defensible — the session was already on the
///   capable tier and nothing escalated — and stated here so it is not
///   rediscovered as a defect.
/// - **The first routed turn of a session can narrate.** `last_decision` is
///   `None`, so nothing was capable before, and an `Override` on turn one means
///   the resent history a client brought with it carried a critical result.
///   There are recent steps and they did show trouble; the note is true.
fn opened_a_tier_escalation(
    recipe: Option<&TierRecipe>,
    decision: &Decision,
    state: &SessionState,
) -> bool {
    // No recipe is every deployment that has not configured one, and it is the
    // first check because it is the cheap one.
    let Some(recipe) = recipe else {
        return false;
    };
    // `is_signal_driven` and not "the tier changed": `DecisionSource::Ambiguous`
    // reaches the capable tier on every turn of a `capable_first` project, and
    // narrating that would tell the model the cheap tier had been stalling on a
    // turn where nothing said it was. Upstream's `only_on_wrong_signal_escalation`,
    // and the one place this tree spells it.
    if !decision
        .source
        .is_some_and(DecisionSource::is_signal_driven)
    {
        return false;
    }
    let capable = recipe.list(Tier::Capable);
    let names_capable = |target: &Target| capable.contains(&target.policy_identity());
    names_capable(&decision.target)
        && !state
            .last_decision()
            .is_some_and(|last| names_capable(&last.chosen))
}

/// One turn's input: the conversation, and what the client said it was talking
/// to.
///
/// **A struct rather than a fifth argument on [`Engine::run_turn`]**, and the
/// reason is the one the declared baseline exists to serve. `model` is a
/// property of the *request*, exactly like the items are, and every caller that
/// has one has the other; a separate parameter would let a surface pass the
/// conversation and forget the baseline, which is a silent downgrade of every
/// counterfactual that turn would have priced. `From<Vec<Item>>` keeps every
/// caller that has nothing to declare — the whole test surface, and the MCP and
/// admin paths — spelling exactly what it spelled before.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnInput {
    pub items: Vec<Item>,
    /// The `model` the client named on the request, verbatim.
    ///
    /// Read by pricing and by nothing else — see
    /// [`DecisionRecord::declared_baseline`](roundhouse_core::routing::DecisionRecord::declared_baseline).
    pub declared_baseline: Option<String>,
}

impl From<Vec<Item>> for TurnInput {
    fn from(items: Vec<Item>) -> Self {
        Self {
            items,
            declared_baseline: None,
        }
    }
}

/// What a local worker produced.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalExecution {
    pub text: String,
    pub output_tokens: u64,
    /// Thinking tokens, already counted inside `output_tokens`.
    ///
    /// A locally served model reasons no less than a hosted one, and the
    /// metrics layer compares the two on this axis, so the local path reports
    /// it rather than assuming zero. An engine that does not separate thinking
    /// from answer leaves this at zero, which reads as "not applicable" and is
    /// the honest answer for a model with no thinking mode.
    pub reasoning_tokens: u64,
}

/// Dispatches a prompt to a chosen local worker.
///
/// Separate from [`LocalFleet`], which only decides *where* a turn should go.
/// Splitting them keeps the routing decision testable without a worker to talk
/// to, and lets the real implementation be swapped for a mock in tests.
///
/// The prompt arrives as token ids rather than text, and that is the invariant
/// the whole cache thesis rests on: these are the exact ids the context
/// assembler hashed into the block and sequence hashes the turn was priced and
/// routed on. Re-tokenizing text here would break that for any real BPE, where
/// `encode(a) + encode(b) != encode(a + b)` at an item boundary — the worker
/// would then prefill a token stream whose blocks hash differently from the
/// ones we matched against, and overlap would be zero forever. Dynamo engines
/// take prompt token ids directly, so passing them through costs nothing.
#[async_trait]
pub trait LocalExecutor: Send + Sync + 'static {
    async fn execute(
        &self,
        endpoint: &str,
        prompt_tokens: &[u32],
        expected_output_tokens: Option<u32>,
    ) -> Result<LocalExecution, FleetError>;
}

/// Deterministic [`LocalExecutor`] for tests and offline runs.
pub struct EchoLocalExecutor {
    reply: String,
}

impl EchoLocalExecutor {
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }
}

#[async_trait]
impl LocalExecutor for EchoLocalExecutor {
    async fn execute(
        &self,
        _endpoint: &str,
        _prompt_tokens: &[u32],
        _expected_output_tokens: Option<u32>,
    ) -> Result<LocalExecution, FleetError> {
        Ok(LocalExecution {
            text: self.reply.clone(),
            output_tokens: self.reply.len() as u64,
            reasoning_tokens: 0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Identity presented to the session lease.
    ///
    /// It must be unique for every live engine. The default mints one so two
    /// server processes cannot accidentally present themselves as one holder.
    pub node_id: String,
    pub lease_ttl_ms: u64,
    pub block_size: u32,
    pub local_model: String,
    pub routing_group: String,
    /// Capability of the local model relative to the frontier catalog.
    pub local_quality_prior: f64,
    /// Latency floor attributed to a local worker before prefill.
    pub local_base_ttft_ms: f64,
    pub expected_output_tokens: u32,
    /// Bounds the model work of a single turn.
    ///
    /// The lease heartbeat keeps an owner alive for as long as its turn runs,
    /// and that is only safe because a turn cannot run forever. Without this
    /// bound a provider that accepts a request and then goes silent would leave
    /// an owner that is alive but producing nothing renewing its lease
    /// indefinitely, and the session would never fail over to anyone able to
    /// make progress. The deadline is what makes the heartbeat safe.
    pub turn_deadline_ms: u64,
    /// What arm assignment is hashed against, deployment-wide.
    ///
    /// Configuration and stable: moving it re-randomizes the experiment, which
    /// is something an operator does deliberately between studies and never in
    /// the middle of one. It reaches the log only as the *arm it produced* —
    /// stamped once into `SessionCreated` and never recomputed — so editing it
    /// changes which arm the next new session lands in and no historical one.
    ///
    /// Empty by default, which is a salt like any other rather than "no
    /// experiment": a deployment with no membership enrolled stamps no arm
    /// whatever this says.
    pub arm_salt: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            node_id: format!("node_{}", uuid::Uuid::new_v4().simple()),
            lease_ttl_ms: 10_000,
            block_size: 16,
            local_model: "local".to_string(),
            routing_group: "default".to_string(),
            local_quality_prior: 0.6,
            local_base_ttft_ms: 60.0,
            expected_output_tokens: 256,
            turn_deadline_ms: 120_000,
            arm_salt: String::new(),
        }
    }
}

/// A dispatch that produced an answer.
struct Completed {
    text: String,
    usage: Usage,
    decision: Decision,
    /// What the provider said this call cost, on the providers that say.
    ///
    /// Beside `usage` and never inside it: `usage` is what this deployment
    /// prices from its own catalog, and this is the external bill that pricing
    /// is later reconciled against. See
    /// `SessionEventKind::ResponseCompleted::provider_reported_cost_usd`.
    provider_reported_cost_usd: Option<f64>,
}

/// How [`Engine::plan`] failed, and the dead dispatch that explains it.
///
/// A struct rather than a bare [`EngineError`] because the failover loop knows
/// one thing the error string does not: *which target* the client's error is
/// about, and how it failed. Every path into `plan` that is not the loop
/// converts through [`From`], so the ordinary `?` sites read exactly as they
/// did and only the one place that has an attempt to carry says so.
struct PlanFailure {
    error: EngineError,
    terminal_attempt: Option<DispatchAttempt>,
}

impl<E: Into<EngineError>> From<E> for PlanFailure {
    fn from(error: E) -> Self {
        Self {
            error: error.into(),
            terminal_attempt: None,
        }
    }
}

/// A dispatch that did not, and everything that survived it.
///
/// The partial and the evidence are not diagnostics; they are what the settle
/// seam commits. The partial is durable output a successor can resume from, and
/// the evidence is what the cache ledger reads to decide whether the target is
/// warm — which is why it must describe what actually happened rather than what
/// was intended.
struct Failed {
    error: EngineError,
    partial: String,
    evidence: Usage,
    /// The dispatch this failure *is*, when it came from a target.
    ///
    /// Carried out of [`Engine::plan`]'s failover loop rather than
    /// reconstructed at the settle seam, because the seam cannot reconstruct
    /// it: the target and the class live in the loop, and the error the loop
    /// returns is a string by the time it crosses the boundary. Every earlier
    /// attempt of the same turn rides the `Routed` of the dispatch it caused;
    /// this is the one that caused none. See
    /// `SessionEventKind::ResponseIncomplete::terminal_attempt`.
    terminal_attempt: Option<DispatchAttempt>,
}

impl Failed {
    /// A failure with nothing to show for it.
    ///
    /// No delta ever arrived, so there is no proof the prompt reached the
    /// provider and the empty usage keeps the ledger's reading of that target
    /// cold. Over-claiming warmth here is the mispricing the evidence rule on
    /// `SessionState`'s pending routings exists to prevent.
    fn before_output(failure: impl Into<PlanFailure>) -> Self {
        let PlanFailure {
            error,
            terminal_attempt,
        } = failure.into();
        Self {
            error,
            partial: String::new(),
            evidence: Usage::default(),
            terminal_attempt,
        }
    }

    /// A failure once the answer had begun.
    ///
    /// A delta cannot exist without a prefill, so a non-empty partial is proof
    /// the whole prompt was processed and the evidence bills it as input. The
    /// output, cached and cache-write counts stay zero: the provider never
    /// reported them, and a fabricated count would be billed to a client as if
    /// measured.
    fn mid_stream(error: impl Into<EngineError>, partial: String, isl_tokens: u64) -> Self {
        let evidence = if partial.is_empty() {
            Usage::default()
        } else {
            Usage {
                input_tokens: isl_tokens,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                // Inferred from the existence of a delta rather than counted
                // by anyone, so it is marked as what it is.
                accounting: Accounting::Estimated,
            }
        };
        Self {
            error: error.into(),
            partial,
            evidence,
            // A body that died after the stream opened names no failed
            // *dispatch*: the target answered, and a second attempt was never
            // an option (see `plan`'s note on why the failover boundary is
            // `execute` and not the stream).
            terminal_attempt: None,
        }
    }

    /// How the log should name this failure when the response is terminated.
    ///
    /// Derived from the error rather than fixed at the construction site,
    /// because the two callers above split on *when* the failure happened and
    /// this splits on *what* failed — a policy refusal and a provider timeout
    /// both arrive through [`Self::before_output`].
    ///
    /// Only the refusal is separated out, and only because it is the one
    /// failure with no upstream in it at all: see
    /// [`IncompleteReason::PolicyRefused`]. Everything else genuinely is an
    /// attempt that did not come back, whatever stage it died at.
    ///
    /// Spelled out variant by variant rather than with a catch-all, which is
    /// what made the budget arm below a decision somebody had to take rather
    /// than a case a wildcard filed under `UpstreamError` on its own.
    ///
    /// Three reasons for three systems, and the split is the whole point:
    /// `budget_exhausted` sends a client to an admin who can raise a limit,
    /// `policy_refused` sends an operator to a control-plane file, and
    /// `upstream_error` sends them to the fleet or the provider. A failure
    /// filed under the wrong one is a person sent to the wrong system.
    fn incomplete_reason(&self) -> IncompleteReason {
        match &self.error {
            EngineError::Routing(RoutingError::PolicyRefused) => IncompleteReason::PolicyRefused,
            // The one failure a retry can legitimately fix without anyone
            // touching a policy: an admin raises the limit, or the month rolls
            // over. See `IncompleteReason::BudgetExhausted`.
            EngineError::BudgetRefused => IncompleteReason::BudgetExhausted,
            // Neither of the other two empty-set arms is a refusal, and each
            // for its own reason. `NoCandidates`: nothing was quoted at all,
            // which is a fleet and a catalog with nothing in them rather than
            // a decision anyone made about this tenant. `NoViableCandidate`:
            // candidates were quoted *and the tenant was entitled to them*,
            // and the deployment's own routing constraints — a load ceiling
            // over a busy fleet — took the rest. Calling that a policy refusal
            // tells the client that widening a policy is the fix for an
            // overloaded worker, and sends the operator to read a `TurnPolicy`
            // that is not the problem. It is the blame this failure carried
            // before M2 split the two.
            EngineError::Routing(
                RoutingError::NoCandidates
                | RoutingError::NoViableCandidate { .. }
                | RoutingError::Policy(_),
            )
            | EngineError::Session(_)
            | EngineError::Fleet(_)
            | EngineError::Frontier(_)
            // Neither spend failure is a *budget* refusal, and filing them as
            // one would tell a client to go and ask for a bigger limit when
            // the budget said nothing at all: `Spend` is this deployment's own
            // ledger being unreachable — the same class of failure as its
            // session store, which is why it sits beside `Session` — and
            // `UnpricedSettlement` is this build having written a hosted
            // decision with no price on it, which is a defect in this process
            // rather than anything a tenant did.
            | EngineError::Spend(_)
            // And the fair-use ledger being unreachable is the same class as
            // `Spend`: this deployment's own store, not a decision about the
            // tenant. It is reachable *here* only in principle — the fair-use
            // check runs at the transport's admission, above `run_turn`, and
            // the draw after the turn is contained — which is why it is filed
            // rather than given a reason of its own that no client would ever
            // see. A refusal by a rolling window is a `429` and never a
            // terminal log event; see `Engine::fair_use_refusal`.
            | EngineError::FairUse(_)
            | EngineError::UnpricedSettlement(_)
            | EngineError::UnresolvableTarget(_)
            | EngineError::TurnDeadline(_) => IncompleteReason::UpstreamError,
        }
    }
}

/// Outcome of one turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub response_id: ResponseId,
    pub text: String,
    /// `None` when no routing happened: the turn was deduplicated onto an
    /// earlier response, or answered at the interjection seam without ever
    /// being planned.
    pub decision: Option<Decision>,
    pub usage: Usage,
    /// Sequence number after the turn; the cursor a client resumes from.
    pub last_seq: u64,
    pub deduplicated: bool,
}

pub struct Engine<S: SessionStore, T: Tokenizer + Clone> {
    store: Arc<S>,
    tokenizer: T,
    fleet: Option<Arc<dyn LocalFleet>>,
    local_executor: Arc<dyn LocalExecutor>,
    frontier_catalog: StaticFrontierCatalog,
    /// Every transport this process can dispatch through, keyed by the
    /// `provider` on the chosen target's catalog entry.
    ///
    /// **A registry and not a client since M10.1.** One client for the whole
    /// catalog was correct while a deployment addressed one origin, and it
    /// stops being correct the moment a single turn's candidate list spans two
    /// — a capable tier on OpenRouter and a fallback on OpenAI's own endpoint
    /// is the shape this phase exists to serve, and it needs two base URLs, two
    /// credentials and two connection pools resolved per dispatch rather than
    /// per process.
    ///
    /// [`Engine::new`] still takes one client and wraps it in the uniform
    /// registry, which is what every test and every pre-M10.1 deployment means
    /// by "the frontier client"; [`Engine::with_provider_clients`] is what the
    /// composition root replaces it with once a catalog has a `providers`
    /// section.
    frontier_clients: Arc<FrontierClients>,
    policy: Arc<dyn RoutingPolicy>,
    config: EngineConfig,
    /// Folds every event this node commits into the dashboard's aggregates.
    ///
    /// Owned by the engine rather than passed in, so a deployment cannot end
    /// up serving turns with no accounting behind them. The fold is a handful
    /// of integer additions per event; there is no configuration under which
    /// leaving it out would be worth the empty dashboard.
    metrics: Arc<MetricsRecorder>,
    /// Where this deployment's committed spend lives.
    ///
    /// Defaulted rather than required, and the default is not a permissive
    /// one: [`MemorySpendLedger`] enforces every ceiling correctly for the
    /// scope it covers, because the race the contract suite closes is between
    /// two *sessions* and not between two processes — the turn gate is
    /// per-session, so a single process can already overspend without this.
    /// What a memory ledger does not survive is a restart, which is the same
    /// property [`MemoryStore`](roundhouse_core::store::MemoryStore) has, and
    /// a deployment that cares wires the durable ledger at the same site it
    /// wires the durable store. `main` does exactly that, and the two move
    /// together on purpose: a durable log beside a ledger that forgets would
    /// re-grant a month's budget on every restart.
    spend: Arc<dyn SpendLedger>,
    /// This deployment's rolling fair-use counters.
    ///
    /// Defaulted rather than required, and — unlike [`Self::spend`] — the
    /// default is the *only* implementation this milestone has. See
    /// [`fair_use`](roundhouse_core::control::fair_use) for what a memory
    /// ledger does not survive and for the unlock condition on the Redis one;
    /// the composition root warns when a deployment has made its sessions
    /// durable while these counters have not.
    ///
    /// A separate field from `spend` rather than a second method on it,
    /// because they are separate stores with separate arithmetic — the whole
    /// content of "fair use is not a `BudgetWindow` variant".
    fair_use: Arc<dyn FairUseLedger>,
    /// What decides whether an admitted turn is dispatched or answered here.
    ///
    /// Never an `Option`. The default occupant
    /// ([`interject::production_default`](roundhouse_core::interject::production_default))
    /// decides `Proceed` and is what ships, so "no interjector configured" and
    /// "an interjector that never interjects" are one state rather than two
    /// spellings of it — and this turn's path has no `None` branch on it that a
    /// reader has to evaluate before believing what the seam does.
    ///
    /// **M6 replaces the occupant and nothing else.** The trigger, the arms,
    /// the judge side-call and the verdict-to-action map all arrive as a type
    /// installed through [`Engine::with_interjector`]; the seam's position in
    /// [`Engine::run_turn`] and its two answers are fixed here.
    interjector: Arc<dyn Interjector>,
    /// The node-local control-plane state the MCP surface writes and this
    /// engine reads.
    ///
    /// Two directions, one store, and they are the two halves of the same
    /// conversation: an agent's overlay is *consumed* here on every turn this
    /// engine goes on to route — not on a turn the interjection seam answers,
    /// which routes nowhere and so has no decision to spend a ration against
    /// (see [`Engine::narrowed_admission`]) — and a steer's corrective payload
    /// is *deposited* here after the log commit that emitted its call. The two
    /// exclusions are one turn: a steered turn deposits and does not consume.
    /// Sharing one `Arc` with the surface is not
    /// an optimization — a surface holding its own copy would install overlays
    /// no turn reads and hold payloads no agent can fetch, and both failures are
    /// silent from every side.
    ///
    /// `None` for a deployment that mounts no control surface, which is a
    /// deployment with nothing to consume and nobody to deposit for. It is an
    /// `Option` rather than an always-present empty store because the difference
    /// is observable in exactly one place — a steered turn whose payload has
    /// nowhere to go — and that is worth a distinct state rather than a silent
    /// write into a map no reader holds.
    control: Option<Arc<ControlStore>>,
    /// One gate per session, held for the whole of [`Engine::run_turn`].
    ///
    /// This gate serializes turns inside one engine before they contend for the
    /// store lease. Across engines, the lease's fencing token is the authority:
    /// every acquisition mints a new tenure and the store rejects stale handles.
    /// Entries are never removed — bounded by the sessions this process serves,
    /// which is acceptable for a single-process skeleton.
    turn_gates: Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    /// Fires the "this recipe routes nothing" warning at most once. (M10.2, S3)
    ///
    /// **The hole conditional composition leaves, stated where it is knowable.**
    /// `main.rs` wraps [`StagePolicy`](roundhouse_core::routing::StagePolicy)
    /// only when some project already had a `tiers` block at boot — see
    /// `tiers_configured` there for why unconditional composition was refused —
    /// so a recipe *added through the admin plane afterwards* lands on a process
    /// whose router cannot read it. The operator gets a config field that
    /// re-routes nothing, with no error and, without this, no log line either.
    ///
    /// **Once per process rather than once per turn**, and the reason is that
    /// the condition is a property of the *composition*, not of the traffic: it
    /// is either true for every turn this process will ever serve or false for
    /// all of them, so a per-turn `warn!` would emit one identical line per
    /// request for the life of the deployment — a volume that trains an operator
    /// to filter exactly the line they need. The remedy is a restart, and one
    /// line survives to the next one.
    unread_recipe: std::sync::Once,
}

impl<S: SessionStore, T: Tokenizer + Clone> Engine<S, T> {
    /// An engine whose whole catalog dispatches through one transport.
    ///
    /// The shape a test with an echo stub means, and the shape
    /// `ROUNDHOUSE_FRONTIER_UPSTREAM` alone produced before M10.1. Kept as the
    /// primary constructor rather than replaced, because a caller with one
    /// client would otherwise have to spell out a registry of one — and because
    /// "one transport for every provider" is a real deployment posture, not a
    /// degenerate case of the registry.
    pub fn new(
        store: Arc<S>,
        tokenizer: T,
        local_executor: Arc<dyn LocalExecutor>,
        frontier_catalog: StaticFrontierCatalog,
        frontier_client: Arc<dyn FrontierClient>,
        policy: Arc<dyn RoutingPolicy>,
        config: EngineConfig,
    ) -> Self {
        Self::with_provider_clients(
            store,
            tokenizer,
            local_executor,
            frontier_catalog,
            Arc::new(FrontierClients::uniform(frontier_client)),
            policy,
            config,
        )
    }

    /// An engine that dispatches each provider's traffic through its own
    /// transport.
    ///
    /// **A constructor and not a builder on [`Self::new`], deliberately.** A
    /// builder would mean an engine briefly held a client it was about to
    /// discard, and a registry composed on top of a uniform fallback would
    /// answer for a provider nobody defined — quietly sending an undefined
    /// provider's turns to whichever origin happened to be the constructor's
    /// argument, which is exactly what the catalog's boot cross-checks exist to
    /// make impossible. Here the registry is the whole answer to "where does
    /// this provider's traffic go", and a name it does not hold is an error.
    #[allow(clippy::too_many_arguments)]
    pub fn with_provider_clients(
        store: Arc<S>,
        tokenizer: T,
        local_executor: Arc<dyn LocalExecutor>,
        frontier_catalog: StaticFrontierCatalog,
        frontier_clients: Arc<FrontierClients>,
        policy: Arc<dyn RoutingPolicy>,
        config: EngineConfig,
    ) -> Self {
        Self {
            store,
            tokenizer,
            fleet: None,
            local_executor,
            frontier_catalog,
            frontier_clients,
            policy,
            config,
            metrics: Arc::new(MetricsRecorder::new()),
            spend: Arc::new(MemorySpendLedger::new()),
            fair_use: Arc::new(MemoryFairUseLedger::new()),
            interjector: roundhouse_core::interject::production_default(),
            control: None,
            turn_gates: Mutex::new(HashMap::new()),
            unread_recipe: std::sync::Once::new(),
        }
    }

    /// The running token and dollar aggregates for everything this node served.
    pub fn metrics(&self) -> Arc<MetricsRecorder> {
        Arc::clone(&self.metrics)
    }

    /// The serialization gate for one session's turns.
    fn turn_gate(&self, session_id: &SessionId) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.turn_gates
                .lock()
                .expect("turn-gate map poisoned")
                .entry(session_id.clone())
                .or_default(),
        )
    }

    pub fn with_fleet(mut self, fleet: Arc<dyn LocalFleet>) -> Self {
        self.fleet = Some(fleet);
        self
    }

    /// Serve budgets out of `spend` instead of this process's own memory.
    ///
    /// A builder rather than a constructor argument, for the reason
    /// [`Self::with_fleet`] is one: it is a deployment's choice of backend and
    /// not a fact every caller has to state. See [`Self::spend`] on why the
    /// default is honest rather than fail-open.
    pub fn with_spend_ledger(mut self, spend: Arc<dyn SpendLedger>) -> Self {
        self.spend = spend;
        self
    }

    /// Count rolling fair-use draws in `fair_use` instead of this process's own
    /// memory.
    ///
    /// A builder for [`Self::with_spend_ledger`]'s reason, and unused by the
    /// shipped binary today: the memory ledger is the only implementation, so
    /// the seam exists for the tests that need a recording one and for the
    /// Redis implementation whose unlock condition is written at
    /// [`FairUseLedger`]. Present now rather than added later because a trait
    /// with exactly one implementation and no way to substitute it is a trait
    /// nobody can prove is a seam.
    pub fn with_fair_use_ledger(mut self, fair_use: Arc<dyn FairUseLedger>) -> Self {
        self.fair_use = fair_use;
        self
    }

    /// Consult `interjector` instead of the production default before each
    /// admitted turn is planned.
    ///
    /// A builder for the same reason [`Self::with_fleet`] and
    /// [`Self::with_spend_ledger`] are: it is a deployment's choice, not a fact
    /// every caller has to state, and the default is a real occupant rather
    /// than an absence — see [`Self::interjector`].
    ///
    /// **Its production caller is M6.** Until the validator exists, the only
    /// thing that installs an occupant is `steering_emission.rs`, which is
    /// where the decision to steer has to come from while there is nothing to
    /// make it: a test-only *type*, but not a test-only code path — the engine
    /// consults this field on every turn either way.
    pub fn with_interjector(mut self, interjector: Arc<dyn Interjector>) -> Self {
        self.interjector = interjector;
        self
    }

    pub async fn create_session(&self, session_id: &SessionId) -> Result<bool, EngineError> {
        Ok(self
            .store
            .create_session(session_id, self.policy.name())
            .await
            .map_err(SessionError::from)?)
    }

    /// A ledger seeded with the frontier catalog's cache models and pricing.
    ///
    /// Seeded before replay so historical dispatches are interpreted under the
    /// right TTL rather than defaulting to "no cache".
    fn seeded_ledger(&self) -> CacheLedger {
        let mut ledger = CacheLedger::new();
        self.frontier_catalog.apply_to_ledger(&mut ledger);
        ledger
    }

    /// Run one turn to completion, charged to `admission`'s principal and
    /// bounded by its policy.
    ///
    /// One [`Admission`] rather than a principal and a policy side by side:
    /// they are resolved together from one key by
    /// [`ControlPlane::turn_admission`](crate::control_config::ControlPlane::turn_admission),
    /// and passing them separately would let a caller pair one tenant's
    /// identity with another tenant's entitlements — a mistake with no
    /// compile-time answer and, since both turns would still serve, no
    /// runtime symptom either. It is also what keeps this signature at the
    /// arity the M1 review set as the ceiling.
    ///
    /// Neither the principal nor the policy is an `Option`: an unconfigured
    /// deployment resolves every request to
    /// [`Admission::open`](crate::control_config::Admission::open), so "a turn
    /// nobody is paying for" and "a turn under no policy" are states no caller
    /// can construct. The budget deliberately *is* one, and it is the only
    /// place in this signature where absence means something: no budget
    /// configured is not an enormous limit, it is a ledger this turn never
    /// calls.
    ///
    /// The principal reaches the log through the session-created event below
    /// and nowhere else; the policy reaches it as the digest on every
    /// [`DecisionRecord`]; the budget reaches it as that record's
    /// `budget_state`, and reaches the spend ledger as a grant and a
    /// settlement. The engine spends all three, it stores none of them.
    pub async fn run_turn(
        &self,
        session_id: &SessionId,
        turn_id: TurnId,
        input: impl Into<TurnInput>,
        admission: &Admission,
    ) -> Result<TurnResult, EngineError> {
        let TurnInput {
            items: input,
            declared_baseline,
        } = input.into();
        // See `turn_gates`: within this node, one turn at a time per session.
        let gate = self.turn_gate(session_id);
        let _turn = gate.lock().await;

        // Observed rather than plain: the session feeds the metrics fold both
        // its replay and its subsequent commits, so a node that restarts and
        // picks a session back up recovers that session's accounting instead
        // of reporting only what it served since booting.
        let mut session = Session::open_observed(
            Arc::clone(&self.store),
            session_id.clone(),
            &self.config.node_id,
            self.config.lease_ttl_ms,
            self.seeded_ledger(),
            Some(Arc::clone(&self.metrics) as Arc<dyn SessionObserver>),
        )
        .await?;

        // The repair, riding on the replay that just happened. A process that
        // died between its log commit and its settle left exactly one turn's
        // spend unapplied, and this is where it is put right — before anything
        // else this turn does, so the grant below is opened against a balance
        // that already includes it.
        //
        // It cannot fail the turn, and that containment is the whole reason it
        // is spelled `repair_settle` rather than `settle` with a `?`: this
        // point is past the lease and before the response, so a failure here
        // has nothing to terminate and would repeat on every open of the same
        // session. See `Engine::repair_settle` for what a skipped repair
        // costs and where a ledger outage is reported instead.
        self.repair_settle(&session, admission).await;

        // Identity, written once, into an empty log. This is the only place in
        // the system that can do it race-free and needs no flag to stay
        // idempotent: the lease is already held, and a log is empty exactly
        // once, so a second caller would have to win the lease *and* still find
        // seq 0. Writing it at `create_session` instead would have to guess at
        // a payer before any credential had been resolved, and writing it from
        // the transport would need a lease the transport deliberately never
        // takes.
        //
        // Because it lands at seq 1 and every replay starts at seq 0, a fold
        // learns whose a session is before any event that can spend money —
        // which is what lets `by_principal` exist without a side table.
        if session.last_seq() == 0 {
            session
                .record_created(
                    self.policy.name(),
                    &admission.principal,
                    self.arm_for(session_id, admission),
                )
                .await?;
        }

        // `started`, not `admission`: the caller's [`Admission`] is who may
        // spend and on what, and this one is whether the log accepted the turn
        // at all. Two unrelated questions that used to share a name because
        // only one of them existed.
        let started = session.begin_turn(turn_id.clone(), input).await?;
        let response_id = started.response_id().clone();
        if let TurnAdmission::Deduplicated(_) = started {
            // A retry of a completed turn. Replay rather than regenerate: the
            // client already paid for this answer, and the accounting it was
            // billed under is durable in the log, so report that rather than a
            // second, fabricated one.
            let text = replay_output(&session, &response_id).await?;
            // Present by construction: the same projection that deduplicated
            // this turn is the one that recorded its usage.
            let usage = session
                .state()
                .completed_usage_for(&turn_id)
                .cloned()
                .unwrap_or_default();
            let last_seq = session.last_seq();
            // Release on this path too. Returning while still holding the lease
            // would lock the session out until the TTL lapsed, turning a
            // harmless client retry into a stall.
            session.release().await?;
            return Ok(TurnResult {
                response_id,
                text,
                decision: None,
                usage,
                last_seq,
                deduplicated: true,
            });
        }

        // Held across dispatch *and* settle. A model call outlives the lease TTL
        // routinely, and without renewal every one of those turns would be
        // fenced at commit and throw away an answer already paid for; the
        // settle's own appends land after the whole stream, so they need the
        // lease just as much. Renewing at a third of the TTL tolerates two lost
        // ticks before a live owner is declared dead.
        let _heartbeat = session.heartbeat(self.config.lease_ttl_ms / 3, self.config.lease_ttl_ms);

        // The interjection seam. Its whole contract — consulted before the turn
        // is planned, with the session's projection and never with the
        // candidate list — is written at [`roundhouse_core::interject`]; what
        // this site owes it is the *position*, and the position is the
        // contract's load-bearing half.
        //
        // After the dedup short-circuit above, so a retry of a turn that
        // already *completed* — every steered turn, by construction — replays
        // the log and never reaches here: whatever the occupant costs is spent
        // once, not once per attempt. (A turn that failed is re-admitted and
        // re-decided, which is right: nothing was answered, so there is no
        // decision to reuse.) After the heartbeat, so an occupant that takes a
        // network round trip cannot lose the lease it is deciding under. And
        // *before* `dispatch`, which is what makes the accounting below true
        // rather than merely tidy — nothing has been priced, no `Routed`
        // exists, and no grant has been opened.
        let side_call_id = SideCallId::generate();
        let interjection = self
            .interjector
            .consider(&InterjectionContext {
                state: session.state(),
                response_id: &response_id,
                turn_policy: &admission.policy,
                // What the agent declared through the MCP surface, and the
                // log's own fallback where it declared nothing. See
                // [`Self::objective`].
                objective: self.objective(session_id, session.state()),
                // Who a check is billed to, under what key, and by what name.
                // Not the candidate list, and never a price: an occupant may be
                // told there is no room for a check and never what the turn it
                // is checking would have cost.
                //
                // The id is minted here, before anyone knows whether a check
                // will happen, because the money question is asked before the
                // call and a hold has to be keyed by something. One id per
                // turn, spent or not, is the cheapest possible way to make the
                // ledger row and the log row name one string — and a turn that
                // never checks simply never uses it.
                side_call: SideCall {
                    session_id,
                    id: &side_call_id,
                    // The log position this turn is being checked at: after its
                    // own `TurnStarted` and before anything the check can
                    // cause, so it rises with every turn and a replay computes
                    // the same number. It is the settle's idempotency key.
                    at_seq: session.last_seq(),
                    principal: &admission.principal,
                    budget: admission.budget.as_ref(),
                },
                // What this membership permits of the loop, resolved from the
                // same key the policy and the budget were. `None` is a
                // membership that is not enrolled, and it releases the turn as
                // surely as an unstamped session does — see
                // `Validator::consider`, which asks both.
                validation: admission.validation.as_ref(),
            })
            .await;
        // The one settle seam. Every admitted turn terminates its response and
        // hands back the lease, whichever way the body went: returning while
        // still holding the lease would lock the session out until the TTL
        // lapsed, and leaving the response open would strand every poller of
        // `is_terminal` and make the next retry duplicate this turn's input
        // forever rather than replay it.
        //
        // Two bodies, one tail. A steered turn and a dispatched one differ in
        // what they commit and in nothing else, so they meet here as the same
        // triple rather than as two functions that each have to remember to
        // settle and release. That is what makes "every terminal event this
        // engine writes goes through the one settle seam" a property of the
        // control flow instead of a claim two call sites keep separately.
        let settled = match interjection {
            // Answered at the seam, never dispatched.
            //
            // **Completed, never incomplete.** Only a completion registers in
            // the session's completed turns, so an incomplete steered turn
            // would re-enter the seam on every retry and never settle — and
            // `response.incomplete` reads as an error in the client rather
            // than as a call to run.
            //
            // **Nothing is booked for the turn.** No `Routed` was recorded,
            // because the seam is consulted before `plan`, so the fold's
            // dispatch-to-terminal pairing finds nothing for this response and
            // books no model row, and the cache ledger records no warm prefix.
            // The `settle` below still runs, and that is deliberate rather
            // than redundant: it prices this terminal event at zero (a
            // settlement carrying no target owes nobody anything) and advances
            // the ledger's per-session watermark, so the *next* turn's repair
            // sees a settle that has already been applied instead of
            // re-driving a turn that never took a grant.
            //
            // **The dashboard total equals the sum of its rows exactly once,
            // and this is the site that makes it true.** A steered turn books
            // no model row for itself, and the judge's side call — committed
            // in `record`, one line down — books once under the judge's own
            // row. Two rows for one turn would double-count the check; none
            // would report a turn that genuinely cost money as free. Without
            // this comment the absence reads as a missing `record_routing`,
            // which is exactly the "fix" that would break it.
            //
            // **The text is what the item says**, and since M10.0 both seam
            // answers say something: a steer carries the guidance and the
            // restated request, a halt carries the guidance alone. Both are
            // assistant text, so `spoken_text` is the whole projection — it
            // still answers the empty string for a tool call, which is what
            // keeps a caller concatenating `text` into a transcript from
            // finding a call in it, but nothing this seam produces is one.
            // See [`Item::spoken_text`](roundhouse_core::item::Item::spoken_text).
            //
            // **The usage booked here is the interjection's, and it is a
            // ledger number rather than a wire number.** Reporting
            // `Usage::default()` instead would make this deployment's own
            // dashboard exceed what clients were told they spent, which is the
            // one direction an accounting error must never run.
            //
            // What this usage is *not* is a measure of the conversation the
            // steer stood in for — and until F03 the Responses surface
            // forwarded it verbatim as `response.completed.usage`, which codex
            // reads as the turn's context contribution and folds into
            // `last_token_usage` (`codex-rs/protocol/src/protocol.rs:2122-2125`
            // and `core/src/context_manager/history.rs:297-314,415-419` at
            // `e363b08`). The two questions now have two answers: the log keeps
            // this one, and [`Engine::context_contribution`] is what the wire
            // reports. That split is why nothing about this line changed and
            // why `a_side_call_books_under_its_own_model_row` is unaffected.
            Interjection::Complete {
                item,
                usage,
                record,
            } => {
                // The record goes in the same append batch as the item and the
                // completion. A decision and its realization committed
                // separately leave a window in which a steered turn exists with
                // nothing in the log saying what decided it.
                let committed = session
                    .complete_with_item(&response_id, item.clone(), usage.clone(), record)
                    .await;
                committed
                    .map(|()| (item.spoken_text().to_string(), usage, None))
                    .map_err(EngineError::from)
            }
            Interjection::Proceed { record } => {
                // Whatever deciding *not* to interject cost, before anything
                // else in this arm: a judge that was consulted and said carry
                // on, or one that could not be reached, is a fact about this
                // turn either way. Committed rather than dropped, because a
                // validator that released the turn and left no trace is
                // indistinguishable from one that never ran — and an empty
                // record, which is what the production default returns, commits
                // nothing at all.
                session.record_control(record).await?;

                // The agent's own narrowing, spent here and applied for the
                // rest of this turn.
                //
                // **Inside this arm, not above the seam.** Consuming before the
                // interjection was the position that made the `Complete` arm
                // above spend a ration for a turn that never reached `plan` —
                // no `Routed`, no `DecisionRecord`, no `turn_policy_digest`, so
                // nothing in the audit trail the charge could be checked
                // against, and `status`'s promise that the digest it reports is
                // the one the next decision carries silently false for every
                // steered turn. Here the claim on
                // [`Self::narrowed_admission`] is not just preserved but
                // finally true: this is still before `plan`, so the turn routed
                // under the overlay is the turn that spent it *by
                // construction*, and now every turn that spends one is a turn
                // that routed.
                //
                // Nothing below the seam needed the narrowed value. `settle`
                // reads the principal and the budget, and `narrow` touches
                // neither — a budget is an admin's ceiling and not an axis an
                // agent may move.
                let admission = &self.narrowed_admission(session_id, admission);
                match self
                    .dispatch(
                        &mut session,
                        &response_id,
                        admission,
                        declared_baseline.as_deref(),
                    )
                    .await
                {
                    Ok(Completed {
                        text,
                        usage,
                        decision,
                        provider_reported_cost_usd,
                    }) => {
                        let committed = session
                            .complete(
                                &response_id,
                                &text,
                                usage.clone(),
                                provider_reported_cost_usd,
                            )
                            .await;
                        committed
                            .map(|()| (text, usage, Some(decision)))
                            .map_err(EngineError::from)
                    }
                    Err(failed) => {
                        // Terminating a failed dispatch is what commits the partial for
                        // a successor to resume from, and what tells the cache ledger
                        // whether the prompt reached the provider. Best-effort: the
                        // usual reason this append fails is a lost lease, and the
                        // original error is the better diagnosis.
                        //
                        // A policy refusal terminates here too, rather than returning
                        // with the response left open: an open response would be
                        // re-admitted on the client's next retry and its input
                        // appended a second time, so "the turn was refused" would
                        // grow the conversation every time the client asked again.
                        let reason = failed.incomplete_reason();
                        let Failed {
                            error,
                            partial,
                            evidence,
                            terminal_attempt,
                        } = failed;
                        let _ = session
                            .mark_incomplete(
                                &response_id,
                                partial,
                                reason,
                                evidence,
                                terminal_attempt,
                            )
                            .await;
                        Err(error)
                    }
                }
            }
        };
        // Money after the log, always: the settle is priced from the terminal
        // event's own usage, so it cannot run until that event exists, and a
        // ledger that moved first would charge for turns whose commit then
        // failed. Its failure is reported *after* the dispatch's, because the
        // dispatch failing is the better diagnosis of the same turn — the same
        // temporal-first-failure rule `local_stream` settles a reservation
        // under.
        let spend = self.settle(&session, admission).await;
        // The rolling counters, after the settle and **not inside it**.
        // `settle` returns early for a membership with no budget, and a project
        // with fair-use windows and no dollar ceiling is exactly the shape this
        // phase ships — a draw hung off the settle would record nothing for the
        // projects fair use actually governs. See `Engine::record_fair_use_draw`.
        self.record_fair_use_draw(&session, &response_id, admission)
            .await;
        let last_seq = session.last_seq();
        // Stop renewing before handing the lease back, so no renewal can land
        // after the release and re-own a session this node has finished with.
        drop(_heartbeat);
        let _ = session.release().await;

        let (text, usage, decision) = settled?;
        spend?;
        Ok(TurnResult {
            response_id,
            text,
            decision,
            usage,
            last_seq,
            deduplicated: false,
        })
    }

    /// Price every option, choose one, record the choice, and execute it.
    ///
    /// Split out so that all of it is fallible in one place: every step here
    /// leaves a response open and a lease held, and [`Engine::run_turn`] is the
    /// only thing allowed to settle either. The error boundary inside is the
    /// moment the stream opens — everything before it fails as a plain
    /// [`EngineError`] with nothing to show, everything after fails as
    /// [`Failed`] carrying the partial — so the conversion happens exactly once,
    /// at the seam [`Engine::plan`] returns through.
    async fn dispatch(
        &self,
        session: &mut Session<S>,
        response_id: &ResponseId,
        admission: &Admission,
        declared_baseline: Option<&str>,
    ) -> Result<Completed, Failed> {
        // One deadline for every model await in this turn, taken before any of
        // them: a provider that hangs after accepting the request settles the
        // turn here rather than leaving a stuck owner renewing its lease. Store
        // appends are deliberately not under it — the heartbeat renews through
        // the same store, so a hung store stops the renewals by itself and
        // cannot keep the lease alive.
        let deadline_at = Instant::now() + Duration::from_millis(self.config.turn_deadline_ms);

        let (mut stream, decision, isl_tokens) = self
            .plan(
                session,
                response_id,
                deadline_at,
                admission,
                declared_baseline,
            )
            .await
            .map_err(Failed::before_output)?;

        // One fold for both targets: a response is a stream of deltas, and each
        // one becomes durable as it arrives rather than at the end. That is
        // what lets a successor resume a half-written answer, and what makes
        // TTFT a measured quantity — the first `OutputTextDelta.at_ms` in the
        // log minus the `Routed.at_ms` before it — instead of the model's own
        // estimate of itself. On a turn that fell forward, "the `Routed` before
        // it" is the *last* one, which is the dispatch that answered: the right
        // reading, since the time a dead provider took to fail is on that
        // provider's own attempt row rather than charged to the model that
        // eventually spoke.
        let mut text = String::new();
        let mut reported: Option<Usage> = None;
        let mut reported_cost_usd: Option<f64> = None;
        loop {
            let chunk = match tokio::time::timeout_at(deadline_at, stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(error))) => {
                    return Err(Failed::mid_stream(error, text, isl_tokens as u64));
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(Failed::mid_stream(
                        self.deadline_struck(),
                        text,
                        isl_tokens as u64,
                    ));
                }
            };
            match chunk {
                FrontierChunk::OutputText(part) => {
                    // Durable before it is accumulated: what the client is told
                    // it received must never be ahead of what the log holds.
                    if let Err(error) = session.append_output(response_id, &part).await {
                        return Err(Failed::mid_stream(error, text, isl_tokens as u64));
                    }
                    text.push_str(&part);
                }
                FrontierChunk::Done {
                    input_tokens,
                    cached_input_tokens,
                    cache_write_tokens,
                    output_tokens,
                    reasoning_tokens,
                    provider_reported_cost,
                } => {
                    // Recorded and not booked, deliberately. A provider's own
                    // dollar figure is the *other* side of the reconciliation
                    // view — the external bill our `committed_usd` is to be
                    // checked against — and folding it into `Usage` here would
                    // put a number nobody derived from the catalog into the
                    // column the savings claim is computed from. It rides the
                    // terminal event to the log instead, which is what makes it
                    // survive the process that measured it; before review
                    // finding G11 it was spent on a `tracing::debug!` that the
                    // binary's own default `info` filter drops, so on any
                    // deployment that did not set `RUST_LOG` the number existed
                    // nowhere at all by the time the turn returned. See
                    // `FrontierChunk::Done::provider_reported_cost`.
                    reported_cost_usd = provider_reported_cost;
                    reported = Some(Usage {
                        input_tokens,
                        cached_input_tokens,
                        // Carried, not derived. The ledger already *prices*
                        // every uncached input token at the cache-write rate;
                        // this is the first time a dispatch tells it how many
                        // were actually written, and the two must stay
                        // distinguishable or the correction can never be made.
                        cache_write_tokens,
                        output_tokens,
                        reasoning_tokens,
                        accounting: Accounting::Reported,
                    });
                }
            }
        }

        // A stream that ended without an accounting chunk is the common case
        // this system has to survive, not an anomaly: a streaming
        // OpenAI-compatible endpoint sends no usage unless the request asked
        // for it, and a gateway in the path can drop it even when it did.
        // Recording `Usage::default()` there would bill the turn as zero
        // tokens for zero dollars, which on a frontier target is
        // indistinguishable from a saving — so the gap is filled from what we
        // do know and stamped as an estimate.
        let usage = reported.unwrap_or_else(|| self.estimated_usage(&text, isl_tokens));

        Ok(Completed {
            text,
            usage,
            decision,
            provider_reported_cost_usd: reported_cost_usd,
        })
    }

    /// What this deployment's tokenizer makes of a conversation, as the input
    /// sequence length of the request that would carry it.
    ///
    /// Public for one caller — the Responses surface, which needs this number
    /// for a turn the interjection seam answers and therefore never dispatches.
    /// It goes through [`ContextAssembler::rehydrate`] with *this engine's*
    /// tokenizer and block size, which is the whole point of exposing it rather
    /// than letting the surface tokenize for itself: [`Self::plan`] prices a
    /// dispatched turn on exactly this quantity, so the number reported for a
    /// steered turn and the number reported for the turn after it are produced
    /// by one function and cannot drift into two conventions.
    ///
    /// Summed per item rather than built through [`Self::assembler_over`], so
    /// this can borrow. An assembler owns what it pushes, and cloning the whole
    /// conversation here would be a `Vec<Item>` copied on *every* request —
    /// including the ordinary dispatched turns that discard this number — to
    /// produce a count. The identity that makes the borrowing form exact rather
    /// than approximate is `ContextAssembler::push`, which is precisely
    /// `encode(item.render())` appended per item; it is pinned by
    /// `the_admitted_input_count_is_what_the_assembler_would_buffer`, which goes
    /// red if that step ever stops being per-item and sends this back to
    /// building the assembler.
    pub fn admitted_input_tokens(&self, items: &[Item]) -> u64 {
        items
            .iter()
            .map(|item| self.tokenizer.encode(&item.render()).len() as u64)
            .sum()
    }

    /// The one place a conversation becomes a priced buffer for this engine.
    ///
    /// Extracted so [`Self::plan`] and any future caller cannot answer "how big
    /// is this conversation" differently — which is the drift that would make a
    /// steered turn's reported input and the next turn's priced input two
    /// conventions instead of one number.
    fn assembler_over(&self, items: Vec<Item>) -> ContextAssembler<T> {
        ContextAssembler::rehydrate(self.tokenizer.clone(), self.config.block_size, items)
    }

    /// What a turn answered at the interjection seam contributed to the
    /// *client's* context, as a [`Usage`] the wire can report.
    ///
    /// **The wire and the ledger answer different questions, and F03 is what it
    /// cost to have them share one number.** The log books what this deployment
    /// spent — for a steered turn, the judge's side call and nothing else, which
    /// is what keeps the dashboard's total equal to the sum of its rows. A
    /// client reads the same field as something else entirely: codex folds
    /// `response.completed.usage` into `last_token_usage`, *replacing* it
    /// (`protocol/src/protocol.rs:2122-2125` at `e363b08`), and that value is
    /// what drives auto-compaction and `get_context_remaining`
    /// (`core/src/context_manager/history.rs:415-419`, which despite its name
    /// reads `last_token_usage`). Reporting the judge's usage there told a real
    /// client its context had collapsed to ~1100 tokens on the very turn before
    /// it resent a ~5700-token history — measured at 5.0x on this box.
    ///
    /// So the wire reports the turn's *context contribution*: the input this
    /// deployment admitted, and the item it emitted. Neither number is money and
    /// neither is booked; [`Accounting::Estimated`] says so, since both come
    /// from our tokenizer rather than from a provider.
    ///
    /// The alternative — leaving the wire number alone and documenting the
    /// one-turn corruption as accepted, since the next real turn resynchronizes
    /// `last_token_usage` — was refused because the turn it corrupts is the
    /// fulfilling turn: the one carrying the largest history the session has
    /// ever held, and so the one most likely to need the compaction the wrong
    /// number suppresses.
    pub fn context_contribution(&self, admitted_input_tokens: u64, emitted: &Item) -> Usage {
        Usage {
            input_tokens: admitted_input_tokens,
            // Nothing was dispatched, so nothing was served from any provider's
            // prefix cache. Zero is the honest answer and also the conservative
            // one: a cached count invented here would understate what the next
            // turn has to prefill.
            cached_input_tokens: 0,
            // And nothing was written into one either, for the same reason:
            // there was no provider call to write it.
            cache_write_tokens: 0,
            // The prompt encoding, not the spoken text: a tool call says nothing
            // to a human but occupies context exactly as `plan` will count it
            // when the client resends it next turn.
            output_tokens: self.tokenizer.encode(&emitted.render()).len() as u64,
            reasoning_tokens: 0,
            accounting: Accounting::Estimated,
        }
    }

    /// Stand in for a provider that reported nothing.
    ///
    /// Input is not really an estimate — it is the prompt this engine
    /// tokenized, hashed, and routed on, so it is the same number the provider
    /// would have counted barring a tokenizer mismatch. Output is a genuine
    /// estimate: our tokenizer over the text we received. Cached input stays
    /// zero because nothing observable here bears on what a remote cache did,
    /// and the conservative direction is the one that understates the saving
    /// rather than inventing it. The cache-*write* count stays zero for the
    /// same reason and one stronger: it is a measurement by definition, so
    /// filling it from anything but a provider's own report would put a guess
    /// in the one column that exists to be checked against a bill.
    fn estimated_usage(&self, text: &str, isl_tokens: usize) -> Usage {
        Usage {
            input_tokens: isl_tokens as u64,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: self.tokenizer.encode(text).len() as u64,
            // Thinking is not recoverable from the visible text: a provider
            // that withheld its accounting also withheld this.
            reasoning_tokens: 0,
            accounting: Accounting::Estimated,
        }
    }

    /// Everything up to the opened stream: price, choose, record, connect.
    ///
    /// Nothing here has produced output yet, so every failure is a plain
    /// [`EngineError`]; [`Engine::dispatch`] converts at its seam.
    async fn plan(
        &self,
        session: &mut Session<S>,
        response_id: &ResponseId,
        deadline_at: Instant,
        admission: &Admission,
        declared_baseline: Option<&str>,
    ) -> Result<(FrontierStream, Decision, usize), PlanFailure> {
        // Rebuild the prompt from the committed log, so what we price is
        // exactly what a successor would reconstruct.
        let assembler = self.assembler_over(session.state().items.clone());
        let isl_tokens = assembler.buffer().isl_tokens();
        let turn_index = session.turn_index().saturating_sub(1);

        // What the session's own tools have been doing, for the tier scorer.
        //
        // **Derived here rather than carried in state**, and that is the whole
        // of S1: the extractor runs over the committed exchanges, which is the
        // same projection `Evidence::of` hands the validate loop, so a
        // successor that picks this session up scores the turn identically.
        // Nothing is stored, nothing is asked of a model, and a deployment with
        // no recipe pays one walk of the fold's item list for a value no policy
        // reads.
        //
        // Computed unconditionally rather than behind `admission.tiers.is_some()`
        // so there is one code path: an empty session yields the default
        // signals, the scorer returns zero, and the picker's default takes the
        // turn — which is exactly what `None` would have done, through the
        // arithmetic instead of through a branch.
        let signals = TurnSignals::from_exchanges(&exchanges(&session.state().items));

        // --- price every option -------------------------------------------
        let local_quote = match &self.fleet {
            Some(fleet) => {
                self.bounded(
                    deadline_at,
                    fleet.price(&FleetQuery::for_buffer(
                        assembler.buffer(),
                        self.config.local_model.clone(),
                        self.config.routing_group.clone(),
                        Some(self.config.expected_output_tokens),
                        Some(session.session_id().to_string()),
                    )),
                )
                .await?
            }
            None => None,
        };

        let mut candidates: Vec<Candidate> = Vec::new();
        if let Some(quote) = &local_quote {
            candidates.push(quote.to_candidate(
                self.config.local_quality_prior,
                self.config.local_base_ttft_ms,
            ));
        }
        candidates.extend(self.frontier_catalog.quote(
            session.ledger(),
            now_ms(),
            isl_tokens as u64,
            self.config.expected_output_tokens as u64,
        ));

        // --- apply the judge's escalation, as far as the pool allows ---------
        //
        // Here rather than beside the overlay in `narrowed_admission`, and the
        // reason is the line above it: the escalation's floor has to be clamped
        // to what the quoted candidates can reach, and nothing upstream of this
        // point has quoted anything. The escalation itself crosses no side
        // channel — it is read from `SessionState::active_escalation`, folded
        // from the `ValidationDecided` in the log, so a successor picking this
        // session up mid-escalation narrows exactly as this process would have.
        //
        // See `escalate_within_reach`: an overlay may refuse a turn because the
        // agent asked for it, and an escalation may not, because nobody did.
        let escalated = control::escalate_within_reach(
            admission,
            session.state().active_escalation(),
            &candidates,
        );
        let admission = &escalated.admission;
        let turn_policy: &TurnPolicy = &admission.policy;

        // --- and narrate a handoff, if this deployment asked us to (R2, S6) ---
        //
        // **Two things can now open a handoff, so the gate is resolved in two
        // places rather than one, and this is the first half.** Until M10.2
        // there was exactly one narrowing worth narrating — the validate loop's
        // escalation, composed twenty lines up — and the whole gate sat here
        // beside it. S6 adds the second: a tier escalation, which the *router*
        // decides and which therefore cannot be known until `choose` returns.
        // The two halves are joined ~150 lines below, immediately before the
        // dispatch loop, and the `append` stays where it always was, at the
        // frontier quote: that is a fact about one wire, and deciding it there
        // is what stops the next transport growing its own copy of a rule that
        // belongs to routing.
        //
        // Resolved here, then:
        //
        // - **the note itself**, which is the only config read in the whole
        //   gate. Absent is the shipped answer, so this is `None` on every turn
        //   of every deployment that has not opted in, and the entire
        //   decoration path costs one `Option` check;
        // - **the validate loop's half of the condition**: an escalation is in
        //   force *and it began on this turn*. A note must ride once per switch
        //   and never accumulate; `this_turn_opened_an_escalation` is folded
        //   from the log, so a successor picking this session up mid-escalation
        //   does not decorate a second time.
        //
        // What is deliberately *not* a further condition is `escalated.clamped`.
        // A clamped escalation still narrowed routing, and an unclamped one
        // whose floor the previous turn already met may have moved nothing — so
        // the flag is both over- and under-inclusive as a proxy for "the target
        // changed". The honest fix is in the wording rather than in the gate:
        // `handoff::EXAMPLE_HANDOFF_NOTE` claims a review found trouble, which
        // is exactly what is true here, and claims nothing about who is
        // answering. See `validate::handoff`.
        let configured_note = admission
            .validation
            .as_ref()
            .and_then(|terms| terms.handoff_note.as_deref());
        let validate_loop_escalated = session.state().this_turn_opened_an_escalation();

        // --- drop what this principal could never use ------------------------
        //
        // The second half of the policy's reach into routing, and it exists for
        // a different reason from the first. `ctx.admissible` inside each
        // `RoutingPolicy` is the *behavior* half: it decides what may be
        // chosen. This is the *accounting* half: `considered` is what the
        // dashboard's counterfactual is priced against — `best_frontier_alternative`
        // reads exactly this list — so leaving an unreachable model in it would
        // report a saving against a model no turn of this principal's could
        // ever have been sent to. That is a cost win the deployment never made,
        // and the one number the whole dashboard is judged by.
        //
        // Filtering only here would not do: a policy is free to reach past the
        // scored pool (`EscalationPolicy`'s audit branch does), so the router
        // has to be able to ask. Filtering only there would not do either, for
        // the reason above. Both, therefore, through one predicate.
        //
        // The question is `permits` and not `admits`, and that is the whole
        // difference between the two calls. A cadence-rationed frontier model
        // is not unreachable, it is not available *this turn* — so it stays in
        // `considered` and the counterfactual against it is true, while
        // `admits` at the routing site sees the session's real history and
        // rations it. A model excluded by the filter or the quality floor is
        // unreachable on every turn of this principal's, and goes.
        let quoted = candidates.len();
        candidates.retain(|candidate| turn_policy.permits(candidate));
        if candidates.is_empty() && quoted > 0 {
            // Not `NoCandidates`: the fleet and the catalog answered, and it is
            // this deployment's own policy that left nothing. Reporting the
            // fleet as empty would send an operator to look at the workers.
            return Err(RoutingError::PolicyRefused.into());
        }

        // --- drop what this principal holds no credential for ----------------
        //
        // **Here, and not in the connect branch**, which is where a first draft
        // put it. The review caught that placement as too late twice over, and
        // both halves are visible in the twenty lines below this one: `payer`
        // is stamped on the `DecisionRecord` that `record_routing` is about to
        // write, and `considered` — the list `best_frontier_alternative` prices
        // a local turn's saving against (`metrics/fold.rs`) — is this filtered
        // set. A provider left in because nobody had a key for it would become
        // the counterfactual every local turn is credited against: a dashboard
        // number invented out of a missing credential.
        //
        // Deliberately *not* the same question the policy filter above asks. A
        // cadence- or budget-excluded model is reachable next turn, so its
        // counterfactual is true and it stays in `considered`. A provider this
        // principal cannot authenticate to is unreachable on every turn, and
        // pricing against it is a claim about money that could never have been
        // spent.
        //
        // Local candidates always survive, which is why a missing credential
        // *degrades* rather than failing — the same shape as budget exhaustion,
        // a served turn plus a marker rather than a 500.
        let reached = admission.credentials.reachable(candidates);
        let withheld_providers = reached.withheld_providers;
        let candidates = reached.candidates;
        if candidates.is_empty() && quoted > 0 {
            // Every option went for want of a credential and nothing local
            // survived to degrade to. Its own error rather than
            // `PolicyRefused`, for the reason that one is not `NoCandidates`:
            // the remedy is a credential, and blaming the policy would send an
            // operator to the wrong file.
            //
            // **Reachable on a deployment that booted clean, and the boot check
            // does not say otherwise.** `unkeepable_promises` asks its question
            // of a project's *configured* policy against the catalog quoted at
            // startup — the only policy, and the only fleet, that exist before
            // any session does. Getting here needs two things at once, and each
            // has a runtime cause that check cannot see:
            //
            // - *no hosted candidate the credentials reach* — a pass-through
            //   project whose caller presented no seat on this request, a
            //   member who has not attached a key, or a credential that went
            //   missing mid-process; and
            // - *no local candidate left to degrade to* — an overlay or an
            //   escalation narrowing this session onto hosted targets (both
            //   compose a second `TurnPolicy`, a turn at a time, above the
            //   filter that runs here), or a fleet with no worker to quote at
            //   this moment.
            //
            // The overlay case is the ordinary one: an agent asks for frontier
            // on a session whose seat is absent, and the pool it asked for is
            // the pool it cannot authenticate to.
            //
            // Degrading is therefore not available — there is nothing left in
            // the pool — and terminating with the credential's own reason is
            // the loudest honest answer. Whenever local capacity *does* survive
            // both filters the turn serves locally instead, and the marker on
            // the decision is what says a provider was withheld.
            return Err(EngineError::Frontier(FrontierError::Credential(
                CredentialError::NoCredential {
                    provider: withheld_providers.join(", "),
                },
            ))
            .into());
        }

        // --- reserve what this turn may spend --------------------------------
        //
        // Between the quotes and the choice, which is the only place it can
        // go: the request is the dearest candidate this key's policy admits,
        // so it needs the quoted set, and what comes back is a ceiling the
        // choice is then made *under*, so it has to precede `choose`.
        //
        // A refusing project stops here rather than in the router. A refusal
        // is a terminal log fact with its own `IncompleteReason`, which is the
        // session layer's business; the router's business is choosing among
        // candidates, and there is nothing to choose from when the answer is
        // "not this turn".
        let budget = self
            .open_grant(session, response_id, &candidates, admission)
            .await?;
        if budget.refuses() {
            return Err(EngineError::BudgetRefused.into());
        }

        // --- choose --------------------------------------------------------
        //
        // One `Option::is_some` on the ordinary path, and a `Once` behind it.
        // See `Engine::unread_recipe`: this is the one moment a process can
        // observe that a project's recipe reaches a router that will not read
        // it, because the recipe arrives on the admission and the router was
        // chosen at boot.
        if admission.tiers.is_some() && !self.policy.reads_tier_recipes() {
            self.unread_recipe.call_once(|| {
                tracing::warn!(
                    policy = self.policy.name(),
                    "a project configures a `tiers` recipe, but this process composed a router \
                     that does not read one, so the recipe is selecting nothing; it was almost \
                     certainly added through the admin plane after boot, and a restart composes \
                     the stage router over it"
                );
            });
        }
        let decision = self
            .bounded(
                deadline_at,
                self.policy.choose(&RoutingContext {
                    session_id: session.session_id(),
                    turn_index,
                    isl_tokens,
                    candidates: &candidates,
                    ledger: session.ledger(),
                    turn_policy,
                    // The turns before this one. `record_routing` below folds
                    // this turn's own dispatch in afterwards, which is what
                    // makes the window a trailing one rather than one that
                    // counts the decision being taken.
                    frontier_history: &session.state().frontier_history,
                    // What the ledger just granted, or `Unlimited` where there
                    // was no ledger to ask. The router applies it as one more
                    // axis of the same admissibility question the policy is
                    // applied through, and produces the overflow state that
                    // exists nowhere upstream of it.
                    budget: &budget,
                    // Derived above from the committed log. `Some` on every
                    // turn including the first, whose signals are simply empty.
                    signals: Some(&signals),
                    // The project's recipe, resolved at admission beside the
                    // policy. `None` on every project that configured none,
                    // which is what makes the stage router a no-op for them.
                    tiers: admission.tiers.as_deref(),
                }),
            )
            .await?;

        // --- the handoff gate's second half (S6) ------------------------------
        //
        // **Bound here, before the dispatch loop, and that placement is
        // load-bearing rather than tidy.** `opened_a_tier_escalation` asks what
        // the *previous* turn was served by, and it asks it of
        // `last_decision` — which, with one `Routed` per dispatch, this turn's
        // own first attempt overwrites. Computed inside the loop, an escalating
        // turn whose first capable target died would see its own record on the
        // second pass, read `was_capable`, and silently drop the note on exactly
        // the turns where a failover happened. One `let` above the loop is the
        // whole fix, and `a_note_survives_a_failover_inside_the_escalating_turn`
        // is what keeps it there.
        //
        // The two halves are `||` and not `&&`: either narrowing on its own is
        // worth telling the answering model about, and a turn that did both
        // still gets exactly one note, because what is gated is the `Option`
        // and `append_handoff_note` is called once per dispatch with it.
        let handoff_note = configured_note.filter(|_| {
            validate_loop_escalated
                || opened_a_tier_escalation(admission.tiers.as_deref(), &decision, session.state())
        });

        // --- dispatch, falling forward while the decision has somewhere to go -
        //
        // **One grant and one settle, however many attempts — but one `Routed`
        // per attempt.** The grant was opened above, before `choose`, and
        // nothing in this loop reopens it: a failover is a second attempt at the
        // *same* turn, so a second grant would let a flaky provider pyramid
        // holds until the ledger refused a turn nobody had spent anything on.
        // That is risk 3 in the plan, and this is the line that answers it.
        //
        // **Every dispatch is recorded before its request goes out**, which is
        // why `record_routing` is inside this loop and not after it. That is the
        // property `SessionState::pending_routings` states — a `Routed` "records
        // an intent rather than a transmission" — and it is what stops a window
        // in which a provider call is in flight, possibly billing, with nothing
        // in the log saying any dispatch was attempted. Writing one record after
        // the loop would have been tidier and would have opened exactly that
        // window; an unexplained provider charge is meant to be a red test here,
        // not an anecdote.
        //
        // Three things fall out of that placement, all of them right:
        //
        // - A turn with no fallbacks writes one `Routed`, in the position it has
        //   always been written in, with an empty `attempts` that is skipped on
        //   the wire — byte-identical to a pre-M10 log.
        // - `frontier_history.record` fires once per attempt, which is exactly
        //   what a cadence asks for: `SessionState::frontier_history` says a
        //   dispatch that failed on the way out still counted as reaching for a
        //   hosted model, and this makes that true without a second mechanism.
        // - The fold's pending dispatch is keyed by response and last-wins, so
        //   the terminal usage pairs with the target that actually transmitted,
        //   and each record's own `rate_card` matches the request it describes.
        //
        // `attempts` therefore carries **one row: the failure this dispatch is a
        // consequence of** — never the cumulative history. A cumulative list
        // would be counted again by the metrics fold on every subsequent record,
        // reporting one dead provider as four.
        //
        // **The boundary is `execute`, not the stream.** Everything the failover
        // classes name — transport, timeout, 408/429/5xx — is decided before a
        // single byte of the body has been decoded, so this loop can see all of
        // them; a body that fails halfway through cannot be retried anywhere,
        // because deltas are durable as they arrive and a second attempt would
        // append a second answer to the same response. Upstream draws the line
        // in the identical place ("streaming body failures happen after the
        // retry boundary and are not retried", `client.rs:326` @ `053a61e`).
        //
        // **A local target does not fail over**, and that is this rung's stated
        // scope rather than an oversight: local capacity fails for reasons a
        // second worker shares (a saturated fleet, a missing quote), and the
        // remedy there is the load axis the router already has.
        let ordered: Vec<Target> = std::iter::once(decision.target.clone())
            .chain(decision.fallbacks.iter().cloned())
            .collect();
        let mut opened: Option<FrontierStream> = None;
        let mut failure: Option<EngineError> = None;
        // The one attempt the next record will carry, if there is a next one.
        let mut preceding: Option<DispatchAttempt> = None;

        for target in ordered {
            let chosen = candidates
                .iter()
                .find(|candidate| candidate.target == target)
                .cloned()
                .ok_or_else(|| EngineError::UnresolvableTarget(target.clone()))?;

            // The credential and the payer for this dispatch, resolved once and
            // read twice — by the record below and by the connect after it.
            // `None` is not a state to fall back from: `reachable` above has
            // already made an unreachable target unchoosable, so a `None` here
            // is a caller that skipped the filter, and defaulting the payer
            // would book somebody else's spend under the deployment's name.
            // Resolved once per attempt rather than once per turn for the same
            // reason it was ever resolved once: two `access_for` calls could
            // straddle a key being attached, and the log would then name a payer
            // the request did not use.
            let access = admission
                .credentials
                .access_for(&target)
                .ok_or_else(|| EngineError::UnresolvableTarget(target.clone()))?;

            session
                .record_routing(
                    response_id,
                    DecisionRecord {
                        chosen: target.clone(),
                        // Appended rather than woven in, exactly as the overflow
                        // valve's note is: an ordinary decision's rationale stays
                        // byte-identical to the one a deployment without the
                        // validate loop writes, and the one turn in a session where
                        // the floor served was not the floor asked for says so
                        // where an operator reads it.
                        rationale: match escalated.clamped {
                            true => decision.rationale.clone() + control::ESCALATION_CLAMPED_NOTE,
                            false => decision.rationale.clone(),
                        },
                        policy: self.policy.name().to_string(),
                        isl_tokens: isl_tokens as u64,
                        expected_prefill_tokens: chosen.expected_prefill_tokens,
                        expected_cost_usd: chosen.expected_cost_usd,
                        considered: candidates.clone(),
                        turn_policy_digest: turn_policy.digest(),
                        // The router's own answer, not re-derived here: it is the
                        // one place `BudgetState::ExhaustedOverflow` is produced,
                        // and reconstructing it from the grant would lose every
                        // overflow.
                        budget_state: decision.budget_state,
                        // The price this turn is going to be settled at, written
                        // down while the catalog that quoted it is still the one
                        // in front of us. Every later reader of this event — this
                        // process's own settle, and a successor's repair — prices
                        // from here rather than from whatever catalog it booted
                        // with, which is what makes the two agree by construction
                        // and what stops an edited price list from re-pricing, or
                        // failing to price at all, a turn that is already over.
                        //
                        // `None` for a local target, which `spec_for` answers by
                        // construction: local capacity is billed in prefill
                        // tokens, not dollars.
                        //
                        // Read off the target that *served*, not the one the policy
                        // named: a turn that fell forward from kimi to sol is
                        // settled at sol's card, and pricing it at the card of a
                        // model that never answered would charge the turn for a
                        // dispatch that produced nothing.
                        //
                        // Read off *this* dispatch's target rather than the policy's
                        // first choice: a turn that fell forward from kimi to sol is
                        // settled at sol's card, and pricing it at the card of a
                        // model that never answered would charge the turn for a
                        // dispatch that produced nothing.
                        rate_card: self
                            .frontier_catalog
                            .spec_for(&target)
                            .map(|spec| spec.pricing),
                        // Whose credential this dispatch spends, and what the
                        // credential filter took out. Both are decided above, in
                        // the same pass that filtered the candidate set — which is
                        // the whole argument for that placement: a payer resolved
                        // in the connect branch is resolved after the record it
                        // belongs on has been written, and a settle would then have
                        // to guess.
                        payer: access.payer,
                        // And whether any of it is roundhouse's money to price,
                        // decided here for the same reason and from the same
                        // resolution. Asked of the *admission* rather than of
                        // `access.credential`, which is the one place the two
                        // differ: a local dispatch under a pass-through project
                        // touches no credential, but the hosted call it displaced
                        // would have been the caller's seat to pay for, so a saving
                        // credited against it is the same invented number the seat
                        // turn's price is. See `Billing::of`.
                        billing: Billing::of(&admission.credentials),
                        // And whether a budget was in force at all, taken from the
                        // same admission the grant was opened against a few lines
                        // up — so "a grant was opened for this turn" and "this turn
                        // is charged" are one fact recorded once, rather than two
                        // reads of a plane that an admin may edit in between. A
                        // project that gains a budget after this turn is over does
                        // not retroactively acquire one here, which is exactly what
                        // a settle driven off the live plane used to believe.
                        budget_draw: admission.budget.as_ref().map(|_| admission.budget_counts),
                        // Empty on every ordinary turn, and skipped on the wire
                        // when it is, so a pre-M7 log's decisions stay
                        // byte-identical. Non-empty, it is the only place in the
                        // log that a project whose credential variable was never
                        // set is distinguishable from one that simply prefers its
                        // own workers.
                        withheld_providers: withheld_providers.clone(),
                        // What the client said it was talking to. Written down and
                        // read only by pricing — no line of routing above this one
                        // has seen it, which is the property that keeps `model` an
                        // accepted-*recorded*-never-routed-on field rather than a
                        // back door onto target selection.
                        declared_baseline: declared_baseline.map(str::to_string),
                        // Exactly the failure this dispatch is a consequence of, and
                        // nothing earlier: see the loop's own comment on why a
                        // cumulative list would report one dead provider as four.
                        attempts: preceding.take().into_iter().collect(),
                    },
                )
                .await?;

            let started = Instant::now();
            match self
                .connect(
                    &target,
                    &assembler,
                    local_quote.as_ref(),
                    &access.credential,
                    handoff_note,
                    session.session_id(),
                    isl_tokens,
                    deadline_at,
                )
                .await
            {
                Ok(stream) => {
                    opened = Some(stream);
                    break;
                }
                Err(ConnectFailure { error, class: None }) => {
                    // The provider answered, or nobody could have: a second
                    // target answers a 401 or an unserializable dialect exactly
                    // the same way.
                    //
                    // **And `preceding` stays as it is, so this failure marks no
                    // attempt.** `DispatchAttempt` carries a class, and there is
                    // no honest class for "not a failover reason" — the three it
                    // has are the three that justify trying somebody else. More
                    // to the point, `failed_attempts` counts *dispatches that
                    // were fallen forward from*, and a 401 or a dialect this
                    // build cannot speak is this deployment's own
                    // misconfiguration; booking it against the provider's row
                    // would report a model as failing when what failed was our
                    // key or our catalog. The boot-time cross-checks are where
                    // those two are supposed to die, and they die there loudly.
                    failure = Some(error);
                    break;
                }
                Err(ConnectFailure {
                    error,
                    class: Some(class),
                }) => {
                    preceding = Some(DispatchAttempt {
                        target,
                        class,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    });
                    failure = Some(error);
                    // Under the *same* deadline, which is what makes a failover
                    // bounded rather than a way to spend N times the turn's
                    // budget. A provider that burned the whole allowance before
                    // failing leaves nothing for the next one, and opening a
                    // connection we know will be cut mid-handshake would add a
                    // record describing our own impatience.
                    if Instant::now() >= deadline_at {
                        failure = Some(self.deadline_struck());
                        break;
                    }
                }
            }
        }

        match opened {
            Some(stream) => Ok((stream, decision, isl_tokens)),
            // **The last failure is the turn's failure, and that is where it is
            // recorded.** Every attempt but the final one rides the record of
            // the dispatch it caused; the final one has no successor to ride, so
            // it arrives as this error and as the `ResponseIncomplete` the settle
            // seam writes from it. Appending a duplicate `Routed` for the sole
            // purpose of carrying one more row would put a second decision in the
            // log for a dispatch that never happened.
            //
            // `unreachable` by construction: the loop leaves `opened` empty only
            // through a branch that sets `failure` first, and `ordered` is never
            // empty because it starts with the target the policy chose.
            // The attempt rides out with the error, because this is the only
            // frame that still has both. `preceding` holds it for the same
            // reason it held every earlier one — it is set by the classified
            // branch of the connect match — and the loop ended without a
            // successor `Routed` to hand it to.
            None => Err(PlanFailure {
                error: failure.unwrap_or_else(|| {
                    unreachable!("a loop that opened nothing recorded why on every path")
                }),
                terminal_attempt: preceding.take(),
            }),
        }
    }

    /// Open a stream to one target. Decides nothing, records nothing.
    ///
    /// Split out of [`Self::plan`] so the failover loop reads as a loop over
    /// targets rather than as a loop wrapped around a two-armed match, and — the
    /// load-bearing half — so that the one place a [`FrontierError`] is still
    /// typed is the place that has to classify it. Once `EngineError::from`
    /// swallows the variant, "was this worth another target" is a question about
    /// a string.
    #[allow(clippy::too_many_arguments)]
    async fn connect(
        &self,
        target: &Target,
        assembler: &ContextAssembler<T>,
        local_quote: Option<&LocalQuote>,
        // The credential the caller already resolved for this target, handed in
        // rather than resolved again: two `access_for` calls could straddle a
        // key being attached, and the decision already written would then name a
        // payer this request did not use.
        credential: &TurnCredential,
        handoff_note: Option<&str>,
        session_id: &SessionId,
        isl_tokens: usize,
        deadline_at: Instant,
    ) -> Result<FrontierStream, ConnectFailure> {
        match target {
            // No failover arm, deliberately — see the loop in `plan`. A local
            // failure is a fleet fact, and the router's load axis is where a
            // second worker is chosen.
            Target::Local { .. } => {
                let quote = local_quote.ok_or_else(|| {
                    ConnectFailure::terminal(EngineError::UnresolvableTarget(target.clone()))
                })?;
                let fleet = self.fleet.as_ref().ok_or_else(|| {
                    ConnectFailure::terminal(EngineError::UnresolvableTarget(target.clone()))
                })?;
                self.local_stream(
                    fleet,
                    quote,
                    assembler.buffer().tokens(),
                    isl_tokens,
                    deadline_at,
                )
                .await
                .map_err(ConnectFailure::terminal)
            }
            Target::Frontier { .. } => {
                // The dialect travels with the request. A client cannot ask the
                // catalog itself — each one holds one transport and one
                // serialization — so whatever it needs to serialize correctly
                // has to arrive in the quote or not at all.
                let spec = self.frontier_catalog.spec_for(target).ok_or_else(|| {
                    ConnectFailure::terminal(EngineError::UnresolvableTarget(target.clone()))
                })?;
                // One call, so the offsets and the string they index into are
                // the same render rather than two that could disagree.
                let (rendered, segment_boundaries) = assembler.rendered_with_boundaries();
                let quote = FrontierQuote {
                    target: target.clone(),
                    wire_protocol: spec.wire_protocol,
                    // **The forwarded request, and only the forwarded request.**
                    // `assembler.rendered()` is a projection of the log, not the
                    // log — so decorating it here leaves the stored items, the
                    // prefix hashes and everything a successor would rebuild
                    // byte-identical whether a deployment configured a note or
                    // not. That is R2's whole safety argument, and it is a
                    // property of *where* this line is rather than of anything
                    // the function does.
                    //
                    // `isl_tokens` above is deliberately the undecorated count:
                    // it is what the pool was quoted against and what the
                    // decision was recorded at, and re-deriving it here would
                    // make the audit trail describe a prompt that never went
                    // anywhere. The note is roundhouse's own paragraph on one
                    // turn in a session, so the understatement is bounded and
                    // one-sided — and a frontier settle prices from the
                    // provider's own reported usage anyway.
                    //
                    // The local branch above takes the token *buffer* rather
                    // than a string and is left undecorated: re-tokenizing a
                    // decorated prompt would desynchronize the buffer from the
                    // block hashes the fleet routes on, which is a real cost for
                    // a narration. Named as a gap rather than hidden — a
                    // deployment whose escalations land locally gets the
                    // narrowing without the note.
                    prompt: match handoff_note {
                        Some(note) => {
                            roundhouse_core::validate::append_handoff_note(rendered, note)
                        }
                        None => rendered,
                    },
                    // **Where a provider that caches only on demand is told the
                    // prefix ends.** Passed through from the assembler rather
                    // than derived here, so the offsets index the render above
                    // and a client slicing on them sends the same bytes
                    // `turn_id_for` hashed. Only the Anthropic client reads
                    // them; every other dialect caches on the steering key
                    // beside them and its request is byte-identical either way.
                    //
                    // A handoff note appended above does not invalidate one of
                    // these: the note goes on the *end*, so every interior
                    // offset still names the same item edge and the note lands
                    // inside the final segment — which is where it belongs, as
                    // the one part of this prompt that is new this turn and
                    // must not be inside the block a breakpoint caches.
                    segment_boundaries,
                    // Stable for the life of the session: providers use it to
                    // steer requests to the same cache node, so varying it
                    // would defeat the hit we just routed on.
                    prompt_cache_key: session_id.to_string(),
                    expected_output_tokens: Some(self.config.expected_output_tokens),
                    // The credential travels here for the same reason the
                    // dialect above does: this is the only argument `execute`
                    // receives. It is the *same* resolution the payer on the
                    // decision came from, read out of one `access_for` above —
                    // two calls could resolve two tiers if a key were attached
                    // between them, and the log would then name a payer the
                    // request did not use.
                    credential: credential.clone(),
                };
                // **The registry is resolved from the spec, not from the
                // process.** `spec.provider` is the same string the catalog's
                // boundary cross-checked against the `providers` section at
                // load, which is what makes this lookup total on a booted
                // deployment rather than a place a turn can discover a
                // misconfiguration. Resolving it here rather than at `choose`
                // keeps one rule: a client is picked by the target that was
                // chosen, never by a target that might have been.
                let client = self
                    .frontier_clients
                    .for_provider(&spec.provider)
                    .map_err(|error| ConnectFailure::terminal(EngineError::from(error)))?;
                // Not `bounded`, and that is the one line the whole failover
                // rests on: `bounded` converts through `EngineError::from`,
                // which erases the variant this classification reads. The
                // deadline is applied here by hand so the `FrontierError`
                // survives long enough to be asked whether it is worth another
                // target — and a deadline strike is deliberately terminal, since
                // there is by definition no time left to try anywhere else.
                match tokio::time::timeout_at(deadline_at, client.execute(&quote)).await {
                    Ok(Ok(stream)) => Ok(stream),
                    Ok(Err(error)) => Err(ConnectFailure {
                        class: error.failover_class(),
                        error: EngineError::Frontier(error),
                    }),
                    Err(_) => Err(ConnectFailure::terminal(self.deadline_struck())),
                }
            }
        }
    }

    /// Run a local worker and present what it produced as a stream.
    ///
    /// An adapter at the boundary rather than streaming: [`LocalExecutor`]
    /// returns a whole [`LocalExecution`], so both chunks here are synthesized
    /// after it returns and no delta lands any earlier than the last token
    /// does. Real local streaming arrives with a worker client that can emit
    /// deltas, and [`LocalExecutor`]'s signature does not change until it does.
    /// Keeping the adapter here means the durable-delta fold is already the
    /// only path any response takes, so that client replaces this function
    /// instead of adding a second way for output to reach the log.
    async fn local_stream(
        &self,
        fleet: &Arc<dyn LocalFleet>,
        quote: &LocalQuote,
        prompt_tokens: &[u32],
        isl_tokens: usize,
        deadline_at: Instant,
    ) -> Result<FrontierStream, EngineError> {
        // Book only now that local has actually won. Had the frontier won, the
        // pending selection would simply expire unclaimed.
        let reservation = self
            .bounded(deadline_at, Arc::clone(fleet).reserve(quote))
            .await?;
        let outcome = tokio::time::timeout_at(
            deadline_at,
            self.local_executor.execute(
                &quote.endpoint,
                prompt_tokens,
                Some(self.config.expected_output_tokens),
            ),
        )
        .await;

        // Settle regardless of outcome, a struck deadline included, and never
        // under the deadline: these are in-process selection-service calls, not
        // model awaits, and bounding them by a deadline that already struck
        // would guarantee the very leak — a reservation dropped unreleased,
        // permanently distorting this worker's load — that settling exists to
        // prevent. Release always runs; the first failure in temporal order
        // (prefill, execution, release) is the one reported.
        let prefill = reservation.prefill_complete().await;
        let released = reservation.release().await;
        prefill?;
        let outcome = outcome.map_err(|_| self.deadline_struck())??;
        released?;

        let cached = isl_tokens.saturating_sub(quote.effective_prefill_tokens);
        Ok(FrontierChunk::whole_response(
            outcome.text,
            isl_tokens as u64,
            cached as u64,
            outcome.output_tokens,
            outcome.reasoning_tokens,
        ))
    }

    /// Run one model-facing future under the turn deadline.
    async fn bounded<T2, E, F>(&self, deadline_at: Instant, future: F) -> Result<T2, EngineError>
    where
        F: Future<Output = Result<T2, E>>,
        EngineError: From<E>,
    {
        match tokio::time::timeout_at(deadline_at, future).await {
            Ok(result) => result.map_err(EngineError::from),
            Err(_) => Err(self.deadline_struck()),
        }
    }

    fn deadline_struck(&self) -> EngineError {
        EngineError::TurnDeadline(self.config.turn_deadline_ms)
    }
}

/// Recover a completed response's text from the log.
///
/// Contents, not [`Item::render`]: the render adds the `<|role|>` prefix the
/// prompt needs, and a client replaying a completed turn must get back the
/// bytes the provider produced rather than the prompt encoding of them.
async fn replay_output<S: SessionStore>(
    session: &Session<S>,
    response_id: &ResponseId,
) -> Result<String, SessionError> {
    Ok(session
        .state()
        .items
        .iter()
        .filter(|item| item.response_id.as_ref() == Some(response_id))
        .map(|item| item.content.render())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::context::ByteTokenizer;
    use roundhouse_core::control::{FrontierHistory, TargetFilter, TurnBudget};
    use roundhouse_core::ids::SessionId;
    use roundhouse_core::routing::{AffinityPolicy, RoutingContext};

    fn local(load: f64) -> Candidate {
        Candidate {
            target: Target::Local {
                worker_id: 1,
                dp_rank: 0,
                model: "llama".into(),
            },
            expected_prefill_tokens: 500.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: 0.0,
            quality_prior: 0.6,
            load: Some(load),
        }
    }

    /// What the router answers, run through the same mapping the settle seam
    /// uses — which is the whole subject: a routing error's blame is only
    /// observable as the reason written into the log.
    async fn logged_reason(
        policy: &dyn RoutingPolicy,
        turn_policy: &TurnPolicy,
        candidates: &[Candidate],
    ) -> IncompleteReason {
        let session_id = SessionId::new("s");
        let ledger = CacheLedger::new();
        let frontier_history = FrontierHistory::default();
        let error = policy
            .choose(&RoutingContext {
                session_id: &session_id,
                turn_index: 1,
                isl_tokens: 1_000,
                candidates,
                ledger: &ledger,
                turn_policy,
                frontier_history: &frontier_history,
                budget: &TurnBudget::Unlimited,
                signals: None,
                tiers: None,
            })
            .await
            .expect_err("every case here empties the pool");
        Failed::before_output(error).incomplete_reason()
    }

    #[tokio::test]
    async fn a_pool_emptied_by_the_policys_own_tuning_is_not_blamed_on_the_tenant() {
        // `max_load` is operator tuning of this `AffinityPolicy` instance --
        // every local worker is busy. Nothing about the *tenant's* entitlements
        // refused anything, so `policy_refused` would send an operator to read
        // a `TurnPolicy` that is not the problem, and would tell the client
        // that widening a policy is what fixes it. It is fleet-shaped
        // exhaustion, which is what `upstream_error` has always meant.
        let candidates = [local(120_000.0)];
        assert_eq!(
            logged_reason(
                &AffinityPolicy::new().with_max_load(50_000.0),
                &TurnPolicy::unrestricted(),
                &candidates,
            )
            .await,
            IncompleteReason::UpstreamError,
        );
    }

    #[tokio::test]
    async fn a_pool_emptied_by_the_turn_policy_is_blamed_on_the_policy() {
        // The control, and the reason the test above is not simply "never say
        // policy_refused": the identical fleet under a filter that names
        // nothing on it is a refusal this deployment made about this tenant,
        // and it is the one terminal reason a retry cannot fix.
        let candidates = [local(0.0)];
        let filtered = TurnPolicy {
            allow: TargetFilter::parse(["mistral/*"]).expect("well-formed"),
            ..TurnPolicy::unrestricted()
        };
        assert_eq!(
            logged_reason(&AffinityPolicy::new(), &filtered, &candidates).await,
            IncompleteReason::PolicyRefused,
        );
    }

    /// The identity [`Engine::admitted_input_tokens`] borrows on.
    ///
    /// It sums `encode(item.render())` per item instead of building a
    /// [`ContextAssembler`], because the assembler owns what it pushes and the
    /// Responses surface would otherwise clone the whole conversation on every
    /// request to produce one count. That substitution is exact only while
    /// `ContextAssembler::push` *is* that step — this is the assertion that
    /// notices if it stops being, at which point the count has to go back
    /// through `assembler_over` and pay the clone.
    #[test]
    fn the_admitted_input_count_is_what_the_assembler_would_buffer() {
        let items = vec![
            Item::system_text("be brief"),
            Item::user_text("what does canonicalize do with a namespace?"),
            Item::assistant_text("it drops it", ResponseId::new("resp_1")),
        ];
        let tokenizer = ByteTokenizer;
        // A block size that does not divide the conversation, so a boundary
        // effect would show up rather than cancel.
        let assembled = ContextAssembler::rehydrate(tokenizer, 7, items.clone())
            .buffer()
            .isl_tokens() as u64;
        let summed: u64 = items
            .iter()
            .map(|item| tokenizer.encode(&item.render()).len() as u64)
            .sum();
        assert_eq!(
            summed, assembled,
            "the per-item sum and the assembler's input sequence length must be \
             one number"
        );
        assert!(summed > 0, "the fixture must have something to count");
    }

    #[tokio::test]
    async fn an_empty_fleet_is_neither_a_refusal_nor_this_deployments_fault_to_report() {
        // `NoCandidates` predates all of this: nothing was quoted at all, which
        // is a fleet and a catalog with nothing in them rather than a decision
        // anyone made about this tenant.
        assert_eq!(
            logged_reason(&AffinityPolicy::new(), &TurnPolicy::unrestricted(), &[]).await,
            IncompleteReason::UpstreamError,
        );
    }
}
