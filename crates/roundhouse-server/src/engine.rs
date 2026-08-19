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
use roundhouse_core::control::{MemorySpendLedger, SpendError, SpendLedger, TurnPolicy};
use roundhouse_core::event::{Accounting, IncompleteReason, SessionObserver, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::Item;
use roundhouse_core::metrics::MetricsRecorder;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{
    CacheLedger, Candidate, Decision, DecisionRecord, RoutingContext, RoutingError, RoutingPolicy,
    Target,
};
use roundhouse_core::session::{Session, SessionError, TurnAdmission};
use roundhouse_core::store::SessionStore;
use roundhouse_core::validate::{SideCall, SteerCapability};
use roundhouse_fleet::{
    FleetError, FleetQuery, FrontierChunk, FrontierClient, FrontierError, FrontierQuote,
    FrontierStream, LocalFleet, LocalQuote, StaticFrontierCatalog,
};
use roundhouse_mcp::ControlStore;
use tokio::time::Instant;

use crate::control_config::Admission;

mod control;
mod spend;

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
}

impl Failed {
    /// A failure with nothing to show for it.
    ///
    /// No delta ever arrived, so there is no proof the prompt reached the
    /// provider and the empty usage keeps the ledger's reading of that target
    /// cold. Over-claiming warmth here is the mispricing the evidence rule on
    /// `SessionState`'s pending routings exists to prevent.
    fn before_output(error: impl Into<EngineError>) -> Self {
        Self {
            error: error.into(),
            partial: String::new(),
            evidence: Usage::default(),
        }
    }

    /// A failure once the answer had begun.
    ///
    /// A delta cannot exist without a prefill, so a non-empty partial is proof
    /// the whole prompt was processed and the evidence bills it as input. The
    /// output and cached counts stay zero: the provider never reported them,
    /// and a fabricated count would be billed to a client as if measured.
    fn mid_stream(error: impl Into<EngineError>, partial: String, isl_tokens: u64) -> Self {
        let evidence = if partial.is_empty() {
            Usage::default()
        } else {
            Usage {
                input_tokens: isl_tokens,
                cached_input_tokens: 0,
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
    frontier_client: Arc<dyn FrontierClient>,
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
}

impl<S: SessionStore, T: Tokenizer + Clone> Engine<S, T> {
    pub fn new(
        store: Arc<S>,
        tokenizer: T,
        local_executor: Arc<dyn LocalExecutor>,
        frontier_catalog: StaticFrontierCatalog,
        frontier_client: Arc<dyn FrontierClient>,
        policy: Arc<dyn RoutingPolicy>,
        config: EngineConfig,
    ) -> Self {
        Self {
            store,
            tokenizer,
            fleet: None,
            local_executor,
            frontier_catalog,
            frontier_client,
            policy,
            config,
            metrics: Arc::new(MetricsRecorder::new()),
            spend: Arc::new(MemorySpendLedger::new()),
            interjector: roundhouse_core::interject::production_default(),
            control: None,
            turn_gates: Mutex::new(HashMap::new()),
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
        input: Vec<Item>,
        admission: &Admission,
    ) -> Result<TurnResult, EngineError> {
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
                // Not yet detected. `Absent` is the honest value while nothing
                // reads the request's tool list: under `SteerChannel::Auto` it
                // degrades a correction to plain guidance, which is the safe
                // direction, and the production default interjects on nothing
                // regardless. Detection belongs at the wire layer against the
                // tool list a request declared, which is §7's milestone.
                capability: &SteerCapability::Absent,
                // Who a check is billed to, and under what key. Not the
                // candidate list, and never a price: an occupant may be told
                // there is no room for a check and never what the turn it is
                // checking would have cost.
                side_call: SideCall {
                    session_id,
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
            // **The text is what the item says, and for a call that is
            // nothing.** A caller that concatenated `text` into a transcript
            // must not find a tool call in it — the call reaches a client as a
            // wire item, not as prose — but a halt's item *is* prose, and it is
            // the whole point of the halt: the guidance that ends the agent's
            // loop and hands control back to a human. Returning the empty
            // string for both would leave the degrade path with its correction
            // in the log and nothing on the wire. See
            // [`Item::spoken_text`](roundhouse_core::item::Item::spoken_text).
            //
            // The usage is the interjection's — reporting `Usage::default()`
            // instead would make this deployment's own dashboard exceed what
            // clients were told they spent, which is the one direction an
            // accounting error must never run.
            Interjection::Complete {
                item,
                usage,
                guidance,
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
                    .map(|()| {
                        // Only once the log holds the call. See
                        // [`Self::deposit_steer`] on why that ordering is the
                        // load-bearing half.
                        self.deposit_steer(session_id, &admission.principal, &item, guidance);
                        (item.spoken_text().to_string(), usage, None)
                    })
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
                let admission = &self.narrowed_admission(session_id, session.state(), admission);
                match self.dispatch(&mut session, &response_id, admission).await {
                    Ok(Completed {
                        text,
                        usage,
                        decision,
                    }) => {
                        let committed = session.complete(&response_id, &text, usage.clone()).await;
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
                        } = failed;
                        let _ = session
                            .mark_incomplete(&response_id, partial, reason, evidence)
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
    ) -> Result<Completed, Failed> {
        // One deadline for every model await in this turn, taken before any of
        // them: a provider that hangs after accepting the request settles the
        // turn here rather than leaving a stuck owner renewing its lease. Store
        // appends are deliberately not under it — the heartbeat renews through
        // the same store, so a hung store stops the renewals by itself and
        // cannot keep the lease alive.
        let deadline_at = Instant::now() + Duration::from_millis(self.config.turn_deadline_ms);

        let (mut stream, decision, isl_tokens) = self
            .plan(session, response_id, deadline_at, admission)
            .await
            .map_err(Failed::before_output)?;

        // One fold for both targets: a response is a stream of deltas, and each
        // one becomes durable as it arrives rather than at the end. That is
        // what lets a successor resume a half-written answer, and what makes
        // TTFT a measured quantity — the first `OutputTextDelta.at_ms` in the
        // log minus the `Routed.at_ms` before it — instead of the model's own
        // estimate of itself.
        let mut text = String::new();
        let mut reported: Option<Usage> = None;
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
                    output_tokens,
                    reasoning_tokens,
                } => {
                    reported = Some(Usage {
                        input_tokens,
                        cached_input_tokens,
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
        })
    }

    /// Stand in for a provider that reported nothing.
    ///
    /// Input is not really an estimate — it is the prompt this engine
    /// tokenized, hashed, and routed on, so it is the same number the provider
    /// would have counted barring a tokenizer mismatch. Output is a genuine
    /// estimate: our tokenizer over the text we received. Cached input stays
    /// zero because nothing observable here bears on what a remote cache did,
    /// and the conservative direction is the one that understates the saving
    /// rather than inventing it.
    fn estimated_usage(&self, text: &str, isl_tokens: usize) -> Usage {
        Usage {
            input_tokens: isl_tokens as u64,
            cached_input_tokens: 0,
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
    ) -> Result<(FrontierStream, Decision, usize), EngineError> {
        let turn_policy: &TurnPolicy = &admission.policy;
        // Rebuild the prompt from the committed log, so what we price is
        // exactly what a successor would reconstruct.
        let assembler = ContextAssembler::rehydrate(
            self.tokenizer.clone(),
            self.config.block_size,
            session.state().items.clone(),
        );
        let isl_tokens = assembler.buffer().isl_tokens();
        let turn_index = session.turn_index().saturating_sub(1);

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
            return Err(EngineError::BudgetRefused);
        }

        // --- choose --------------------------------------------------------
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
                }),
            )
            .await?;

        let chosen = candidates
            .iter()
            .find(|candidate| candidate.target == decision.target)
            .cloned()
            .ok_or_else(|| EngineError::UnresolvableTarget(decision.target.clone()))?;

        // Recorded before execution: a decision that led to a failure is still
        // part of the audit trail.
        session
            .record_routing(
                response_id,
                DecisionRecord {
                    chosen: decision.target.clone(),
                    rationale: decision.rationale.clone(),
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
                    rate_card: self
                        .frontier_catalog
                        .spec_for(&decision.target)
                        .map(|spec| spec.pricing),
                },
            )
            .await?;

        // --- connect -------------------------------------------------------
        let stream = match &decision.target {
            Target::Local { .. } => {
                let quote = local_quote
                    .as_ref()
                    .ok_or_else(|| EngineError::UnresolvableTarget(decision.target.clone()))?;
                let fleet = self
                    .fleet
                    .as_ref()
                    .ok_or_else(|| EngineError::UnresolvableTarget(decision.target.clone()))?;
                self.local_stream(
                    fleet,
                    quote,
                    assembler.buffer().tokens(),
                    isl_tokens,
                    deadline_at,
                )
                .await?
            }
            Target::Frontier { .. } => {
                // The dialect travels with the request. A client cannot ask the
                // catalog itself — it holds one `Arc<dyn FrontierClient>` for
                // providers whose transports have nothing in common — so
                // whatever it needs to serialize correctly has to arrive in the
                // quote or not at all.
                let spec = self
                    .frontier_catalog
                    .spec_for(&decision.target)
                    .ok_or_else(|| EngineError::UnresolvableTarget(decision.target.clone()))?;
                let quote = FrontierQuote {
                    target: decision.target.clone(),
                    wire_protocol: spec.wire_protocol,
                    prompt: assembler.rendered(),
                    // Stable for the life of the session: providers use it to
                    // steer requests to the same cache node, so varying it
                    // would defeat the hit we just routed on.
                    prompt_cache_key: session.session_id().to_string(),
                    expected_output_tokens: Some(self.config.expected_output_tokens),
                };
                self.bounded(deadline_at, self.frontier_client.execute(&quote))
                    .await?
            }
        };

        Ok((stream, decision, isl_tokens))
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
