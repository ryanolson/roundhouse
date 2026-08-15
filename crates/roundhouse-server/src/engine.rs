// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turn execution.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use roundhouse_core::context::{ContextAssembler, Tokenizer};
use roundhouse_core::event::{Accounting, IncompleteReason, SessionObserver, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::metrics::MetricsRecorder;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{
    CacheLedger, Candidate, Decision, DecisionRecord, RoutingContext, RoutingError, RoutingPolicy,
    Target,
};
use roundhouse_core::session::{Session, SessionError, TurnAdmission};
use roundhouse_core::store::SessionStore;
use roundhouse_fleet::{
    FleetError, FleetQuery, FrontierChunk, FrontierClient, FrontierError, FrontierQuote,
    FrontierStream, LocalFleet, LocalQuote, StaticFrontierCatalog,
};
use tokio::time::Instant;

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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            node_id: "node-0".to_string(),
            lease_ttl_ms: 10_000,
            block_size: 16,
            local_model: "local".to_string(),
            routing_group: "default".to_string(),
            local_quality_prior: 0.6,
            local_base_ttft_ms: 60.0,
            expected_output_tokens: 256,
            turn_deadline_ms: 120_000,
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
}

/// Outcome of one turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub response_id: ResponseId,
    pub text: String,
    /// `None` when the turn was deduplicated and no routing happened.
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
    /// One gate per session, held for the whole of [`Engine::run_turn`].
    ///
    /// The lease fences *nodes*; it deliberately re-grants to its own node so a
    /// recovering process is not locked out by its previous life. Within one
    /// node that leniency would let two concurrent turns on a session both pass
    /// admission and interleave their writes, so turns are serialized here.
    /// Entries are never removed — bounded by the sessions this process serves,
    /// which is acceptable for a single-process skeleton. The cross-process
    /// version of this guarantee is a fencing token on [`Lease`]
    /// (`roundhouse_core::store::Lease`) that every store call validates; that
    /// replaces this map when the Redis store arrives.
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

    /// Run one turn to completion.
    pub async fn run_turn(
        &self,
        session_id: &SessionId,
        turn_id: TurnId,
        input: Vec<Item>,
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

        let admission = session.begin_turn(turn_id.clone(), input).await?;
        let response_id = admission.response_id().clone();
        if let TurnAdmission::Deduplicated(_) = admission {
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

        let outcome = self.dispatch(&mut session, &response_id).await;

        // The one settle seam. Every admitted turn terminates its response and
        // hands back the lease, whichever way the body went: returning while
        // still holding the lease would lock the session out until the TTL
        // lapsed, and leaving the response open would strand every poller of
        // `is_terminal` and make the next retry duplicate this turn's input
        // forever rather than replay it.
        let settled = match outcome {
            Ok(Completed {
                text,
                usage,
                decision,
            }) => {
                let committed = session.complete(&response_id, &text, usage.clone()).await;
                committed
                    .map(|()| (text, usage, decision))
                    .map_err(EngineError::from)
            }
            Err(Failed {
                error,
                partial,
                evidence,
            }) => {
                // Terminating a failed dispatch is what commits the partial for
                // a successor to resume from, and what tells the cache ledger
                // whether the prompt reached the provider. Best-effort: the
                // usual reason this append fails is a lost lease, and the
                // original error is the better diagnosis.
                let _ = session
                    .mark_incomplete(
                        &response_id,
                        partial,
                        IncompleteReason::UpstreamError,
                        evidence,
                    )
                    .await;
                Err(error)
            }
        };
        let last_seq = session.last_seq();
        // Stop renewing before handing the lease back, so no renewal can land
        // after the release and re-own a session this node has finished with.
        drop(_heartbeat);
        let _ = session.release().await;

        let (text, usage, decision) = settled?;
        Ok(TurnResult {
            response_id,
            text,
            decision: Some(decision),
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
    ) -> Result<Completed, Failed> {
        // One deadline for every model await in this turn, taken before any of
        // them: a provider that hangs after accepting the request settles the
        // turn here rather than leaving a stuck owner renewing its lease. Store
        // appends are deliberately not under it — the heartbeat renews through
        // the same store, so a hung store stops the renewals by itself and
        // cannot keep the lease alive.
        let deadline_at = Instant::now() + Duration::from_millis(self.config.turn_deadline_ms);

        let (mut stream, decision, isl_tokens) = self
            .plan(session, response_id, deadline_at)
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
    ) -> Result<(FrontierStream, Decision, usize), EngineError> {
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
