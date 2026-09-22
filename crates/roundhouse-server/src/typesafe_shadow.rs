// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Classification admission, content limits, and evaluation accounting.
//!
//! Calls require explicit opt-in and an admitted frontier target.
//! [`TypeSafeShadow::projection`] limits the content, and
//! [`TypeSafeShadow::prepare`] serializes the request and records its quote.
//! Preparation performs no ledger or HTTP request. The engine writes the
//! durable intent before a background worker calls [`TypeSafeShadow::execute`].
//! Execution obtains a budget hold, sends the request, and attempts settlement.
//! One absolute deadline covers all three, so no ledger wait can outlive the
//! call it belongs to. An unanswered intent establishes uncertainty, not proof
//! that a call was paid, and an abandoned ledger wait is the same kind of
//! statement: this process stopped waiting, not that the backend undid anything.
//!
//! The engine writes intent and result records under its existing session lease.
//! A background worker must not acquire another writer and fence the live turn.
//! The [projection](roundhouse_core::classify::projection) contains bounded prior
//! metadata and classifications plus the current user prompt. Raw history, tool
//! results, and system instructions are excluded. An oversized rendered payload
//! is rejected to preserve its structure and content boundaries.
//!
//! Reported token usage is priced with the configured rate card. This amount is
//! separate from a provider invoice and from the ledger's [`SettlementAck`].
//! A settlement that failed, or that ran out of time, does not erase observed
//! usage or establish zero cost.
//!
//! The intent retains the configured [`ClassifierIdentity::model`]. Results
//! retain `reported_model` separately when the service supplies a valid identity.
//! This distinction preserves evidence when the requested and returned IDs differ.

use std::collections::BTreeMap;
use std::sync::Arc;

use roundhouse_core::classify::projection::PROJECTION_REVISION;
use roundhouse_core::classify::projection::{PromptCapture, TurnProjection, project};
use roundhouse_core::classify::{
    AvailableClassification, ClassificationAxis, ClassificationIntent, ClassificationOutcome,
    ClassificationRecord, ClassifierIdentity, ContextDependence, EvaluationSpend, EvaluationUsage,
    FundingRefusal, Graded, PriorTurnMetadata, ProjectionCaps, ReservationRecord, SettlementAck,
    TAXONOMY_VERSION, TurnClassification, TurnComplexity, TurnIntent, UnconfirmedSettlement,
};
use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{
    BudgetTerms, BudgetWindow, GrantRequest, Principal, Settlement, SettlementKey, SpendLedger,
    TurnCredential,
};
use roundhouse_core::event::Usage;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::routing::Target;
use roundhouse_core::routing::ledger::ProviderPricing;
use roundhouse_fleet::typesafe::{
    ChoiceAnswer, ChoiceQuestion, PreparedRequest, SignalError, SystemOneClient, SystemOneError,
    SystemOneRequest,
};

use crate::engine::spend::GRANT_TTL_SLACK_MS;

/// The wire schema this adapter asks under, recorded on every intent.
///
/// A string rather than a version integer because it names *somebody else's*
/// shape: a reader comparing two records needs to know which service's request
/// format produced them, not only that this deployment's count moved.
pub const REQUEST_SCHEMA: &str = "typesafe.systemone.choice.v1";

/// What one deployment decided about turn classification.
///
/// No [`Default`], and `enabled` is private: the only way to get an enabled
/// config is to name a model, a rate card, a configuration revision and every
/// cap, and then say so.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowConfig {
    enabled: bool,
    /// The pinned model id. Never defaulted — `jev-latest` re-ranks itself
    /// underneath a deployment that never changed a line.
    pub model: String,
    /// Configuration, not measurement. Rate cards never go in source.
    pub pricing: ProviderPricing,
    /// What the estimate is asked with. These answers are structured choices,
    /// so it is small on purpose.
    pub expected_output_tokens: u64,
    pub caps: ProjectionCaps,
    /// The operator's own revision of this file.
    ///
    /// Stamped on every intent so two records taken under two edits of the same
    /// configuration are distinguishable. Nothing here derives it: a digest
    /// would say two turns differed and never how, and a revision the operator
    /// writes is a number they can look up.
    pub config_revision: u32,
}

impl ShadowConfig {
    /// Disabled, whatever else is configured.
    pub fn new(
        model: impl Into<String>,
        pricing: ProviderPricing,
        expected_output_tokens: u64,
        caps: ProjectionCaps,
        config_revision: u32,
    ) -> Self {
        Self {
            enabled: false,
            model: model.into(),
            pricing,
            expected_output_tokens,
            caps,
            config_revision,
        }
    }

    /// Opt in. The one way `enabled` becomes true.
    pub fn enable(mut self) -> Self {
        self.enabled = true;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn identity(&self) -> ClassifierIdentity {
        ClassifierIdentity {
            model: self.model.clone(),
            schema: REQUEST_SCHEMA.to_string(),
            taxonomy_version: TAXONOMY_VERSION,
            projection_revision: PROJECTION_REVISION,
            config_revision: self.config_revision,
        }
    }
}

/// Why no call was made. Every arm is a refusal taken before any HTTP, and
/// every arm leaves no durable record — nothing was committed to, so there is
/// nothing to be uncertain about later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRun {
    /// No deployment opted in. The default.
    Disabled,
    /// The decision's admitted pool held no frontier target, so this session's
    /// content has no third party it is already permitted to reach.
    /// Conservative on purpose: a local-only session makes zero calls.
    NoAdmittedFrontier,
    /// The decision carries no admission evidence at all.
    ///
    /// **Refused rather than resolved.** A policy outside the builtin set can
    /// assemble a `Decision` without going through
    /// [`Admitted`](roundhouse_core::routing::Admitted), and asking `admissible`
    /// again here would answer a *different* question — with a guessed load
    /// ceiling, without the overflow valve — and then record its answer as this
    /// decision's permission. Silence cannot be read as consent to egress.
    AdmissionUnknown,
    /// The bounded projection still did not fit. Rejected, never cut.
    PayloadTooLarge {
        limit_bytes: usize,
        actual_bytes: usize,
    },
    /// The transport would refuse this request before a socket: a credential
    /// that is not this deployment's to spend, or a body over the wire bound.
    ///
    /// Decided here rather than in the worker because it needs no I/O at all:
    /// everything the transport can refuse without a socket is an eligibility
    /// question, and an ineligible call should leave no durable intent.
    Refused(SystemOneError),
}

/// Everything one call is held and settled under.
///
/// Supplied by the caller rather than minted here: the durable intent the engine
/// writes has to agree with it, and a module that invented its own identity
/// would be a second answer to a question that record already answers.
pub struct ShadowCall<'a> {
    pub principal: Principal,
    pub session_id: SessionId,
    /// The ledger key this call's hold is taken under, and the identity its
    /// settle is deduplicated by. Distinct from any turn's, and distinct per
    /// attempt: a settled identity can never settle again.
    pub call_id: ResponseId,
    /// The turn being classified.
    pub source_turn_index: u64,
    pub source_response_id: ResponseId,
    pub terms: BudgetTerms,
    pub credential: &'a TurnCredential,
    pub now_ms: u64,
    /// Absolute, and binding on the queue wait as well as the HTTP call.
    pub expires_at_ms: u64,
}

/// What a settle needs, owned, so it outlives the borrow the credential came on.
#[derive(Debug, Clone)]
struct SettleContext {
    principal: Principal,
    session_id: SessionId,
    call_id: ResponseId,
    window: BudgetWindow,
}

/// One classification, serialized and checked, with nothing sent and no hold
/// taken.
///
/// The engine writes [`Self::intent`] durably and then hands the whole value to
/// a worker, which opens the grant and sends. It deliberately holds no borrow:
/// the worker outlives the turn.
///
/// `Debug` through [`PreparedRequest`]'s own, which elides the body and keeps
/// its length: a dropped call must not put a prompt in a log.
#[derive(Debug)]
pub struct PreparedCall {
    prepared: PreparedRequest,
    settle: SettleContext,
    terms: BudgetTerms,
    /// The record the engine must commit before this call may be executed.
    pub intent: ClassificationIntent,
}

impl PreparedCall {
    pub fn call_id(&self) -> &ResponseId {
        &self.intent.call_id
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.intent.expires_at_ms
    }
}

/// The policy boundary in front of one System One upstream.
pub struct TypeSafeShadow<T: Tokenizer> {
    client: SystemOneClient,
    config: ShadowConfig,
    /// Required — there is no unbudgeted path through this type.
    ///
    /// Whether this is a *separate* ledger from the project's is the composition
    /// root's to arrange and cannot be checked here: a `SpendLedger` handed in
    /// is indistinguishable from the turn's. `shared_backend::open` is what
    /// makes the separation real, and `Backends::evaluation_spend` is what a
    /// test asserts it on.
    spend: Arc<dyn SpendLedger>,
    tokenizer: T,
}

impl<T: Tokenizer> TypeSafeShadow<T> {
    pub fn new(
        client: SystemOneClient,
        config: ShadowConfig,
        spend: Arc<dyn SpendLedger>,
        tokenizer: T,
    ) -> Self {
        Self {
            client,
            config,
            spend,
            tokenizer,
        }
    }

    pub fn config(&self) -> &ShadowConfig {
        &self.config
    }

    /// The questions this module asks: one per taxonomy axis, in one request.
    ///
    /// **About the turn, never about the models.** No option here is a model id,
    /// a tier or a price — the same rule `validate::brief` holds for the judge,
    /// and for the same reason: the routing question is asked exactly once, of
    /// code. What comes back describes the work, and a selector maps it.
    ///
    /// Built from the axis definitions rather than written out, so a new option
    /// is added in one place and the request, the parser and the durable record
    /// cannot disagree about what was offered.
    pub fn questions() -> BTreeMap<String, ChoiceQuestion> {
        fn axis<A: ClassificationAxis>() -> (String, ChoiceQuestion) {
            (
                A::KEY.to_string(),
                ChoiceQuestion {
                    instructions: A::INSTRUCTIONS.to_string(),
                    criteria: A::options()
                        .iter()
                        .map(|(label, rubric)| (label.to_string(), rubric.to_string()))
                        .collect(),
                },
            )
        }
        BTreeMap::from([
            axis::<TurnIntent>(),
            axis::<TurnComplexity>(),
            axis::<ContextDependence>(),
        ])
    }

    /// The bounded projection, rendered, or why it cannot be sent.
    pub fn projection(
        &self,
        capture: &PromptCapture,
        prior: &[AvailableClassification],
        local: &[PriorTurnMetadata],
    ) -> Result<TurnProjection, NotRun> {
        project(capture, prior, local, &self.config.caps).map_err(|error| NotRun::PayloadTooLarge {
            limit_bytes: error.limit_bytes,
            actual_bytes: error.actual_bytes,
        })
    }

    /// Serialize the call and hand back everything a worker needs — having
    /// sent nothing, asked no ledger anything, and awaited no I/O at all.
    ///
    /// **Synchronous on purpose.** This is the one part of a classification
    /// that runs on the serving turn, because the durable intent has to be
    /// written by the writer the turn already holds. Everything it does is
    /// local: a bounded render, a serialization, a tokenization and a header
    /// build. The grant is the worker's.
    ///
    /// `admitted` is the pool the *policy's own* admission returned, carried on
    /// the decision this turn recorded. `None` is unknown and refuses: see
    /// [`NotRun::AdmissionUnknown`].
    pub fn prepare(
        &self,
        call: ShadowCall<'_>,
        projection: &TurnProjection,
        admitted: Option<&[Target]>,
    ) -> Result<PreparedCall, NotRun> {
        if !self.config.enabled {
            return Err(NotRun::Disabled);
        }
        let Some(admitted) = admitted else {
            return Err(NotRun::AdmissionUnknown);
        };
        // The pool the policy actually resolved, after credential, tool, policy
        // and budget filtering. Requiring an admitted frontier target
        // conservatively excludes local-only sessions from hosted
        // classification, which is the whole of T6's local-only clause.
        if !admitted.iter().any(|target| !target.is_local()) {
            return Err(NotRun::NoAdmittedFrontier);
        }

        let request = SystemOneRequest {
            model: self.config.model.clone(),
            state: projection.rendered.clone(),
            questions: Self::questions(),
        };
        // Everything the transport can refuse without a socket, refused before
        // an intent exists: an ineligible call should leave no durable record
        // of a commitment nobody made.
        let prepared = match self.client.prepare(&request, call.credential) {
            Ok(prepared) => prepared,
            Err(error) => return Err(NotRun::Refused(error)),
        };

        let estimated_input_tokens = self.tokenizer.encode(prepared.body()).len() as u64;
        let requested_usd = self.price(EvaluationUsage {
            input_tokens: estimated_input_tokens,
            output_tokens: self.config.expected_output_tokens,
        });
        let settle = SettleContext {
            principal: call.principal.clone(),
            session_id: call.session_id.clone(),
            call_id: call.call_id.clone(),
            window: call.terms.budget.window,
        };
        let hold_ttl_ms = self.hold_ttl_ms(&call);

        Ok(PreparedCall {
            prepared,
            settle,
            intent: ClassificationIntent {
                call_id: call.call_id,
                source_turn_index: call.source_turn_index,
                source_response_id: call.source_response_id,
                requested_at_ms: call.now_ms,
                expires_at_ms: call.expires_at_ms,
                identity: self.config.identity(),
                reservation: ReservationRecord {
                    rate_card: self.config.pricing,
                    estimated_input_tokens,
                    expected_output_tokens: self.config.expected_output_tokens,
                    requested_usd,
                    hold_ttl_ms,
                    budget_limit_usd: call.terms.budget.limit_usd,
                    budget_window: call.terms.budget.window,
                    member_ceiling_usd: call.terms.member_ceiling_usd(),
                    warn_at: call.terms.budget.warn_at,
                },
            },
            terms: call.terms,
        })
    }

    /// Fund one prepared call, send it once, and settle what it cost.
    ///
    /// **The whole of a classification's I/O, and none of it is on a turn.**
    /// The grant, the request and the settle all happen here, on a background
    /// worker, after the intent is already durable.
    ///
    /// `deadline` is the call's absolute expiry as a runtime instant, and every
    /// wait inside here binds against it: the grant, the request, and the
    /// settle. A call that spent most of its life queued does not then get a
    /// fresh full transport deadline, and it does not get an unbounded ledger
    /// round trip on either side of the request either — a worker parked in one
    /// of those holds its admission permit with no path to ever producing a
    /// result, which is the leak the bound closes.
    ///
    /// **What an abandoned ledger wait does and does not establish.** Stopping
    /// waiting is not a rollback. A grant abandoned at the deadline may still
    /// have opened a hold, and a settle abandoned at it may still have been
    /// applied; what this process knows is that it never received the
    /// acknowledgement. Either hold lapses on its own TTL, which is the
    /// backend's cleanup window rather than a second deadline for this worker.
    /// Everything the call *did* establish before the deadline — the answer,
    /// the reported usage, what this deployment's rate card prices it at, and
    /// the identity that answered — survives onto the record.
    ///
    /// **No retries.** The docs advise backing off on 429 and 529; a
    /// classification nobody is waiting on buys a later answer at a second
    /// charge, and a fresh attempt would need a fresh call identity and a fresh
    /// durable intent anyway.
    pub async fn execute(
        &self,
        call: PreparedCall,
        deadline: tokio::time::Instant,
    ) -> ClassificationRecord {
        let PreparedCall {
            prepared,
            settle,
            terms,
            intent,
        } = call;
        let reservation = &intent.reservation;
        let outcome = match self
            .reserve(
                &settle,
                &terms,
                reservation,
                roundhouse_core::now_ms(),
                deadline,
            )
            .await
        {
            Err(reason) => ClassificationOutcome::Unfunded { reason },
            Ok(granted_usd) => {
                self.send_and_settle(prepared, &settle, granted_usd, deadline)
                    .await
            }
        };
        // Keyed off the intent rather than off the settle context: the record
        // and the intent name one call, and reading the identity from two places
        // is how they come to disagree.
        ClassificationRecord {
            call_id: intent.call_id,
            source_turn_index: intent.source_turn_index,
            source_response_id: intent.source_response_id,
            // **Taken after the call, not before it.** Read at dispatch this
            // would date a sixty-second call to the moment it was queued, and
            // every latency question asked of the log afterwards would be
            // answered with the wrong number.
            completed_at_ms: roundhouse_core::now_ms(),
            outcome,
        }
    }

    /// The funded half: one request, then the settle its answer implies.
    async fn send_and_settle(
        &self,
        prepared: PreparedRequest,
        settle: &SettleContext,
        granted_usd: f64,
        deadline: tokio::time::Instant,
    ) -> ClassificationOutcome {
        // The remaining life of the call, over the transport's own deadline.
        // Whichever is shorter binds, which is what makes the expiry absolute.
        let sent = match tokio::time::timeout_at(deadline, self.client.send(prepared)).await {
            Ok(sent) => sent,
            Err(_) => Err(SystemOneError::DeadlineExceeded),
        };
        match sent {
            Err(error) => {
                // Nothing priceable came back, so a release at zero is
                // submitted — the attempt, not its outcome: whether the ledger
                // acknowledged it is what `settled` carries. The record says
                // the accounting is unknown rather than claiming a free call.
                let settled = self
                    .settle(settle, 0.0, roundhouse_core::now_ms(), deadline)
                    .await;
                ClassificationOutcome::Failed {
                    reason: transport_reason(&error).to_string(),
                    spend: EvaluationSpend::Unknown {
                        granted_usd,
                        settled,
                    },
                }
            }
            Ok(reply) => {
                // Two statements, kept apart on purpose. The ledger is sent zero
                // when nothing priceable came back, because that is how a hold is
                // released; the record says `Unknown`, because a measured zero
                // would book a billed call as free.
                let usage = reply.usage.map(|usage| EvaluationUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                });
                let usd = usage.map(|usage| self.price(usage));
                let settled = self
                    .settle(
                        settle,
                        usd.unwrap_or(0.0),
                        roundhouse_core::now_ms(),
                        deadline,
                    )
                    .await;
                let spend = match (usage, usd) {
                    (Some(usage), Some(usd)) => EvaluationSpend::Measured {
                        usage,
                        usd,
                        granted_usd,
                        settled,
                    },
                    _ => EvaluationSpend::Unknown {
                        granted_usd,
                        settled,
                    },
                };
                // Carried onto every arm an envelope reached, usable answers or
                // not: the identity that answered is a fact about the call and
                // not about whether this deployment could read the reply. The
                // transport has already dropped it if the service echoed the
                // credential back under it.
                let reported_model = reply.reported_model;
                match reply.answers {
                    Ok(answers) => match Self::read(&answers) {
                        Some(classification) => ClassificationOutcome::Classified {
                            classification,
                            spend,
                            reported_model,
                        },
                        // The transport checked every key against the question it
                        // was asked under, so this is unreachable unless that
                        // contract changes. A non-panicking failure retains the
                        // accounting either way.
                        None => ClassificationOutcome::Unusable {
                            reason: "answer_not_in_taxonomy".to_string(),
                            spend,
                            reported_model,
                        },
                    },
                    Err(signal) => ClassificationOutcome::Unusable {
                        reason: signal_reason(signal).to_string(),
                        spend,
                        reported_model,
                    },
                }
            }
        }
    }

    /// Re-drive one settlement the log records as unconfirmed, and say what
    /// the ledger answered.
    ///
    /// **A settle and nothing else.** No grant: a hold opened under an
    /// already-settled identity can never settle again and only expires, so
    /// re-reserving here would park a call's quote against the project's
    /// ceiling for a whole TTL in exchange for nothing. No request either —
    /// there is no answer to buy, only accounting to finish.
    ///
    /// **Every input comes off the durable record**, which is what makes this
    /// safe to run arbitrarily late. The amount is the one the result carried,
    /// the window is the one the intent recorded, and the identity is the one
    /// the hold was opened under; the live rate card is deliberately not in
    /// scope, the same rule `engine::spend::settled_cost_usd` is under and for
    /// the same reason — a repaired charge that disagreed with the charge it
    /// replaced is drift nobody can see without reading both.
    ///
    /// `None` means the ledger did not answer and the settlement stays
    /// unrepaired, to be driven again by a later turn. That costs one more
    /// deduplicated ledger call and, emphatically, no second purchase.
    /// `Some(applied)` is an answer either way: `false` says the ledger already
    /// held this call, which resolves the ambiguity rather than failing.
    pub async fn repair_settlement(
        &self,
        principal: &Principal,
        session_id: &SessionId,
        settlement: &UnconfirmedSettlement,
        deadline: tokio::time::Instant,
    ) -> Option<bool> {
        let settled = tokio::time::timeout_at(
            deadline,
            self.spend.settle_grant(Settlement {
                principal: principal.clone(),
                // The same key the original settle used, so this *is* that
                // settle rather than a second one. Under a watermark it would
                // be dropped as a replay and the drop would look like success.
                key: SettlementKey::OncePerCall,
                response_id: settlement.call_id.clone(),
                actual_usd: settlement.usd,
                window: settlement.window,
                // The operation clock, not the original call's. A settle
                // applies a realized amount at the moment it is applied, so a
                // first charge recovered after a window reset lands in the
                // window that is open now — which is the existing contract and
                // not a historical bucket this path invents.
                now_ms: roundhouse_core::now_ms(),
            }),
        )
        .await;
        match settled {
            Ok(Ok(settled)) => Some(settled.applied),
            Ok(Err(error)) => {
                tracing::warn!(
                    %error,
                    session_id = %session_id,
                    call_id = %settlement.call_id,
                    "an evaluation settlement could not be repaired; it stays \
                     unconfirmed in the log for a later turn to drive again"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    session_id = %session_id,
                    call_id = %settlement.call_id,
                    "an evaluation settlement's repair did not answer inside \
                     its own bound; it stays unconfirmed for a later turn"
                );
                None
            }
        }
    }

    /// Every axis, or nothing.
    ///
    /// **All or nothing, deliberately.** A partial taxonomy is not a weaker
    /// classification, it is a different one: a record missing its context axis
    /// would be indistinguishable from one whose context axis is `unknown`, and
    /// those are the two states this vocabulary exists to keep apart.
    fn read(answers: &BTreeMap<String, ChoiceAnswer>) -> Option<TurnClassification> {
        fn axis<A: ClassificationAxis>(
            answers: &BTreeMap<String, ChoiceAnswer>,
        ) -> Option<Graded<A>> {
            let answer = answers.get(A::KEY)?;
            Some(Graded {
                value: A::from_label(&answer.choice)?,
                confidence: answer.confidence,
            })
        }
        Some(TurnClassification {
            taxonomy_version: TAXONOMY_VERSION,
            intent: axis::<TurnIntent>(answers)?,
            complexity: axis::<TurnComplexity>(answers)?,
            context_dependence: axis::<ContextDependence>(answers)?,
        })
    }

    /// How long the hold survives without a settle.
    ///
    /// The call's own remaining life plus the same slack a turn's grant takes.
    /// **The slack is the backend's cleanup window, not extra time for this
    /// worker**, and the distinction is the whole reason the number is written
    /// down here rather than inferred: every wait in [`Self::execute`] ends at
    /// the call's own expiry, so by the time this slack matters this process has
    /// already stopped asking. What it buys is that a hold whose settle was
    /// abandoned — or whose settle was applied but never acknowledged — lapses
    /// on its own rather than sitting against the ceiling forever. Reading it as
    /// permission to keep settling past the call's expiry is what turns one
    /// deadline into two.
    fn hold_ttl_ms(&self, call: &ShadowCall<'_>) -> u64 {
        call.expires_at_ms.saturating_sub(call.now_ms) + GRANT_TTL_SLACK_MS
    }

    /// Price two axes through the one definition of what a call costs.
    ///
    /// `cached_input_tokens` is zero because this service reports no cache
    /// term — an absent measurement, not a claim that nothing was cached.
    fn price(&self, usage: EvaluationUsage) -> f64 {
        self.config.pricing.price(&Usage {
            input_tokens: usage.input_tokens,
            cached_input_tokens: 0,
            output_tokens: usage.output_tokens,
            ..Default::default()
        })
    }

    /// Hold the quote, or refuse. Answers what the ledger actually held.
    ///
    /// **On the worker, never on a turn.** This is the round trip the serving
    /// path used to await; moving it here is what makes the evaluation budget's
    /// latency invisible to a response.
    ///
    /// **Both of its ledger waits are under the call's own `deadline`**, and
    /// the second one is the easier to miss: a partial grant is handed back
    /// through a zero-dollar settle on a path where no request will ever be
    /// sent, so a bound drawn around the grant and the send alone leaves a
    /// worker able to park there forever on behalf of a call the budget already
    /// refused.
    async fn reserve(
        &self,
        settle: &SettleContext,
        terms: &BudgetTerms,
        reservation: &ReservationRecord,
        now_ms: u64,
        deadline: tokio::time::Instant,
    ) -> Result<f64, FundingRefusal> {
        let grant = match tokio::time::timeout_at(
            deadline,
            self.spend.open_grant(GrantRequest {
                principal: settle.principal.clone(),
                session_id: settle.session_id.clone(),
                response_id: settle.call_id.clone(),
                requested_usd: reservation.requested_usd,
                ttl_ms: reservation.hold_ttl_ms,
                terms: terms.clone(),
                now_ms,
            }),
        )
        .await
        {
            Ok(Ok(grant)) => grant,
            // Fail closed. A ledger that cannot be reached has not said there
            // is room, and a call made anyway is an unbudgeted one.
            Ok(Err(_)) => return Err(FundingRefusal::LedgerUnavailable),
            // The call's life ran out inside the grant. Nothing was sent, so
            // there is no provider charge — which is what
            // [`ClassificationOutcome::Unfunded`] carrying no spend at all
            // says. It is deliberately *not* a statement that the ledger opened
            // no hold: abandoning the wait cannot un-open one, and whichever
            // way that landed it lapses on `hold_ttl_ms`.
            Err(_) => return Err(FundingRefusal::Expired),
        };
        // **A zero grant is an ordinary ledger answer, not an error**, so a
        // refusal spelled as `open_grant(..).is_err()` would send this request
        // unfunded against an exhausted budget. A grant of *less* than the
        // quote is equally a refusal: the prompt is already written and its
        // price is not negotiable downwards, so a partial reservation buys
        // nothing and is handed straight back rather than left to lapse.
        if grant.granted_usd < reservation.requested_usd {
            // Bounded like every other ledger wait, and its answer is
            // deliberately not read: the refusal below is established by the
            // grant, and whether the release was acknowledged changes neither
            // of its two amounts.
            self.settle(settle, 0.0, now_ms, deadline).await;
            return Err(FundingRefusal::BudgetRefused {
                requested_usd: reservation.requested_usd,
                granted_usd: grant.granted_usd,
            });
        }
        Ok(grant.granted_usd)
    }

    /// Release this call's hold, commit what it spent, and say which happened.
    ///
    /// Never propagates: a settle that cannot be applied is a warning and a
    /// skip, the same rule `judge.rs` is under. What that costs is one call's
    /// spend unconfirmed and its hold left to lapse on the TTL — and, since this
    /// answers [`SettlementAck::Unconfirmed`], the durable record says so instead of
    /// reporting the charge as committed.
    ///
    /// **Bounded by the call's own `deadline`, and running out of time answers
    /// the same `Rejected`.** A settle abandoned at the deadline is one this
    /// process never got an acknowledgement for; it is not one this process
    /// knows the backend did not apply, and the record must not be read as
    /// saying otherwise. Giving this its own later deadline — the hold's TTL,
    /// say — would be a second deadline, and a worker under it can outlive the
    /// call it is settling for arbitrarily long while holding the admission
    /// permit that call was admitted under.
    ///
    /// The warning identifies the session and call so an operator can locate the
    /// unconfirmed settlement. It excludes the projection and the credential.
    async fn settle(
        &self,
        settle: &SettleContext,
        actual_usd: f64,
        now_ms: u64,
        deadline: tokio::time::Instant,
    ) -> SettlementAck {
        let settled = tokio::time::timeout_at(
            deadline,
            self.spend.settle_grant(Settlement {
                principal: settle.principal.clone(),
                // **Not the session's watermark.** Several of these are in
                // flight under one session and finish in whatever order their
                // upstreams answer, so a settle keyed by log position would
                // read the call that finished last but was issued first as a
                // replay — dropping its charge and stranding its hold.
                key: SettlementKey::OncePerCall,
                response_id: settle.call_id.clone(),
                actual_usd,
                window: settle.window,
                now_ms,
            }),
        )
        .await;
        match settled {
            Ok(Ok(_)) => SettlementAck::Committed,
            Ok(Err(error)) => {
                tracing::warn!(
                    %error,
                    session_id = %settle.session_id,
                    call_id = %settle.call_id,
                    "an evaluation call's spend could not be committed; leaving its \
                     hold to lapse rather than retrying against an unknown ceiling"
                );
                SettlementAck::Unconfirmed
            }
            Err(_) => {
                tracing::warn!(
                    session_id = %settle.session_id,
                    call_id = %settle.call_id,
                    "an evaluation call's settle did not answer inside the call's \
                     deadline; whether the ledger applied it is unknown, and its \
                     hold is left to lapse rather than retried"
                );
                SettlementAck::Unconfirmed
            }
        }
    }
}

/// A stable short token for a transport failure.
///
/// The variant's name and never its payload: a status body can quote the request
/// back, and a `reqwest` message can carry the configured URL. Both are the
/// things this module's errors exist not to print, and a durable record is the
/// worst possible place to start.
fn transport_reason(error: &SystemOneError) -> &'static str {
    match error {
        SystemOneError::Credential(_) => "credential",
        SystemOneError::ForwardedCredentialRefused => "forwarded_credential_refused",
        SystemOneError::NoQuestions => "no_questions",
        SystemOneError::RequestTooLarge { .. } => "request_too_large",
        SystemOneError::Transport {
            timed_out: true, ..
        } => "transport_timeout",
        SystemOneError::Transport { .. } => "transport",
        SystemOneError::Status { .. } => "status",
        SystemOneError::ResponseTooLarge { .. } => "response_too_large",
        SystemOneError::Malformed => "malformed",
        SystemOneError::DeadlineExceeded => "deadline_exceeded",
    }
}

/// The same treatment for an answer set that arrived and cannot be used.
fn signal_reason(signal: SignalError) -> &'static str {
    match signal {
        SignalError::MissingAnswer => "missing_answer",
        SignalError::UnexpectedAnswer => "unexpected_answer",
        SignalError::NotAChoice => "not_a_choice",
        SignalError::OptionsDisagree => "options_disagree",
        SignalError::ProbabilityOutOfRange => "probability_out_of_range",
        SignalError::SumIsNotOne => "sum_is_not_one",
        SignalError::ChoiceNotOffered => "choice_not_offered",
        SignalError::ConfidenceOutOfRange => "confidence_out_of_range",
    }
}

#[cfg(test)]
mod tests;
