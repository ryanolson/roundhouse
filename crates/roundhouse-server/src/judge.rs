// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The validate loop's side call, made through the fleet the engine already
//! has.
//!
//! [`FleetJudge`] is the production [`JudgeClient`]: a hand-built
//! [`FrontierQuote`] through the existing [`FrontierClient::execute`], with no
//! new transport, no second catalog and no second credential path. What makes
//! it a *side* call rather than a turn is four isolations, each of which is a
//! thing this file deliberately does or deliberately cannot do.
//!
//! **Its own cache key**, `{session_id}#validate`. The conversation's key is
//! kept stable for the life of a session so a provider steers its turns to one
//! cache node, and the router prices the resulting hit; a judge prompt sent on
//! that key would cool the hit the router just quoted. A key of its own that is
//! still stable across validations means the judge's own prefix warms instead,
//! so the marginal cost of checking falls with use.
//!
//! **Its own deadline**, a bounded fraction of the turn's. The checker must
//! never break the checked, so this deadline binds first and by construction:
//! it is taken from the same `turn_deadline_ms` the engine bounds the turn
//! with, multiplied by a fraction below one. A judge that hangs costs the turn
//! that fraction and then releases it.
//!
//! Messages requests mark the system prefix for caching. The target's catalog
//! entry supplies the requested lifetime through
//! [`FrontierModelSpec::requested_cache_ttl_ms`]. The reservation uses the
//! configured cold-write price. A marker does not establish a cache hit:
//! provider eligibility and retention rules still apply.
//!
//! **Its own budget question.** If the payer's ledger cannot cover the check,
//! the check does not happen and the turn proceeds — [`JudgeFailure::Unaffordable`],
//! which the occupant records as `NotRun { BudgetRefused }`. Never fail a turn
//! because we could not afford to check it.
//!
//! That question is a **grant and a settle**, the same discipline a turn is
//! under, and it has to be: a budget that is only ever *read* answers the same
//! way on the first check and the thousandth, because nothing a check spends
//! ever reaches the counter the read consults. That is not a ceiling, it is a
//! per-call price comparison — one check is affordable, so every check is
//! affordable, forever.
//!
//! This path was once a read, for a reason worth recording because it is the
//! constraint the shape here answers. The ledger's holds are keyed by
//! [`ResponseId`], and a side call opens no response and writes no terminal
//! event, so a hold it could take and could not close would strand a turn's
//! worth of a project's money for a TTL on *every* validation. That key now
//! arrives on the [`SideCall`]: the hold is keyed by the check's own
//! [`SideCallId`], which cannot collide with any turn's, and the settle names
//! the same id — once, whatever else happens to that session. And it is closed
//! on every path out of [`FleetJudge::consult`] — the answer, the provider
//! error, the deadline — because there is exactly one exit after the grant.
//!
//! What it costs is bounded and named: a check whose *process* dies mid-call
//! leaves a hold to lapse on its TTL and its provider-side cost uncommitted,
//! and an abandoned check settles at zero because nothing this deployment can
//! price came back. A third case runs the other way — a check whose answer
//! arrives but whose log commit is fenced by a lost lease is committed here and
//! recorded nowhere, because the money left regardless of who won the lease.
//! All three show up where every other measured-versus-committed gap does, in
//! the reconciliation view.
//!
//! Holding rather than reading also closes the concurrency window this file used
//! to name: two turns of one membership can no longer both be told there is room
//! for a check when there is room for one, because the first one's reservation
//! is placed before the second one's grant reads the balance.
//!
//! [`ResponseId`]: roundhouse_core::ids::ResponseId
//! [`SideCallId`]: roundhouse_core::ids::SideCallId
//! [`SideCall`]: roundhouse_core::validate::SideCall
//!
//! **Never the cache ledger.** There is no [`CacheLedger`] in scope in this
//! file and no way to reach one from a [`SideCall`], so the isolation is
//! structural rather than remembered: feeding a judge prompt to the ledger
//! would record a warm prefix on that target for the *conversation's* next
//! turn, and the router would then price a hit nobody can serve.
//!
//! [`CacheLedger`]: roundhouse_core::routing::CacheLedger

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{
    BudgetTerms, GrantRequest, Settlement, SettlementKey, SpendLedger, TurnCredential,
};
use roundhouse_core::event::{Accounting, CacheReadSource, SideCallAbandonReason, Usage};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::now_ms;
use roundhouse_core::routing::Target;
use roundhouse_core::validate::{JudgeAnswer, JudgeClient, JudgeFailure, SideCall};

use crate::engine::spend::GRANT_TTL_SLACK_MS;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec, FrontierQuote, FrontierStream,
};

/// The suffix that makes the side call's cache key its own.
///
/// A constant rather than a literal at the one site that builds the key,
/// because the property the whole isolation rests on is that this string is
/// *not* the conversation's key and *is* the same on every validation. Both
/// halves are asserted against this name.
pub const VALIDATE_CACHE_SUFFIX: &str = "#validate";

/// What joins the two prompts into the one string the transport takes.
const PROMPT_SEPARATOR: &str = "\n\n";

/// One prompt shared by token estimation and transport. Tokenizing its parts
/// separately can miss separators and tokens that span the join.
struct PreparedPrompt {
    text: String,
    /// The system prefix ends here. Empty components produce no boundary,
    /// because the quote requires offsets strictly inside the prompt.
    boundaries: Vec<usize>,
}

impl PreparedPrompt {
    /// Keep the review instructions before the transcript. The prefix includes
    /// the fixed separator, so segmentation preserves the exact sent bytes.
    fn new(system_prompt: &str, brief: &str) -> Self {
        let text = format!("{system_prompt}{PROMPT_SEPARATOR}{brief}");
        let boundaries = match system_prompt.is_empty() || brief.is_empty() {
            true => Vec::new(),
            false => vec![system_prompt.len() + PROMPT_SEPARATOR.len()],
        };
        Self { text, boundaries }
    }
}

/// What a deployment sets about the side call itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JudgeConfig {
    /// The fraction of the turn's deadline the check may take.
    ///
    /// Below one, always, and that is the whole of "the checker must never
    /// break the checked": a check that could run as long as the turn would
    /// let a hung judge consume the entire budget the turn needed to answer.
    /// Clamped rather than refused at [`FleetJudge::new`] — a miscalibrated
    /// fraction is a slower check, and a panic here would take down the turn
    /// this exists not to break.
    pub deadline_fraction: f64,
    /// What the judge is expected to answer with, in tokens.
    ///
    /// Used twice and for two different things: to size the estimate the
    /// budget question is asked with, and to tell the provider what to expect.
    /// A structured verdict is four fields, so this is small on purpose.
    pub expected_output_tokens: u32,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            deadline_fraction: 0.25,
            expected_output_tokens: 128,
        }
    }
}

/// The production [`JudgeClient`]: one hand-built quote through the fleet.
///
/// Generic over the tokenizer for the reason the engine is: the estimate the
/// budget question is asked with must be counted the way this deployment
/// counts everything else, or a deployment with a real BPE would meter its
/// checks against a byte count.
pub struct FleetJudge<T: Tokenizer + Clone> {
    client: Arc<dyn FrontierClient>,
    /// Where the ledger question is asked. `None` for a deployment with no
    /// ledger — which is not "unlimited", it is "nothing to ask", the same
    /// distinction the engine's grant path draws.
    spend: Option<Arc<dyn SpendLedger>>,
    /// The one model the judge runs on, with its rate card.
    ///
    /// A single spec rather than a catalog and a routing policy, and that is
    /// the family-bias rule applied to the judge itself: choosing the judge
    /// per call would put a routing decision inside the validate loop, where
    /// the router's own admissibility rules are not consulted. Which model
    /// judges is a deployment's decision, taken once, in configuration.
    spec: FrontierModelSpec,
    tokenizer: T,
    /// The turn deadline this deployment bounds a turn with, which the check's
    /// own deadline is a fraction of.
    turn_deadline_ms: u64,
    config: JudgeConfig,
}

impl<T: Tokenizer + Clone> FleetJudge<T> {
    pub fn new(
        client: Arc<dyn FrontierClient>,
        spec: FrontierModelSpec,
        tokenizer: T,
        turn_deadline_ms: u64,
        config: JudgeConfig,
    ) -> Self {
        Self {
            client,
            spend: None,
            spec,
            tokenizer,
            turn_deadline_ms,
            config: JudgeConfig {
                // A fraction at or above one is a check that may outlive the
                // turn it is checking, and a fraction at or below zero is a
                // check that times out before it starts. Both are clamped into
                // a usable band rather than refused, for the reason on the
                // field: nothing on this path may take a turn down.
                deadline_fraction: config.deadline_fraction.clamp(0.01, 0.9),
                ..config
            },
        }
    }

    /// Ask `spend` whether a check is affordable before making one.
    ///
    /// A builder rather than a constructor argument, for the reason
    /// [`Engine::with_spend_ledger`](crate::Engine::with_spend_ledger) is one:
    /// it is a deployment's choice of backend. A judge with no ledger asks
    /// nobody and checks every turn the trigger fires on, which is the correct
    /// behavior for a deployment that meters nothing.
    pub fn with_spend_ledger(mut self, spend: Arc<dyn SpendLedger>) -> Self {
        self.spend = Some(spend);
        self
    }

    fn target(&self) -> Target {
        self.spec.target()
    }

    /// What we are about to send, counted the way this deployment counts
    /// everything else.
    ///
    /// Counted once and used twice — to size the budget question and to stand
    /// in for a provider that reports no accounting — because those are the
    /// same number and computing it twice is how they stop being one. See
    /// [`Self::drain`] on what booking the second use at zero cost.
    ///
    /// Taken over [`PreparedPrompt::text`] and never over the two halves: the
    /// string counted here is the string the transport sends, separator
    /// included.
    fn counted_input_tokens(&self, prepared: &PreparedPrompt) -> u64 {
        self.tokenizer.encode(&prepared.text).len() as u64
    }

    /// What this check is expected to cost, before it is made.
    ///
    /// Uses the configured tokenizer, output ceiling, and cold-write rate.
    /// Provider token accounting can differ, so this is not a guaranteed bill cap.
    fn estimated_cost_usd(&self, input_tokens: u64) -> f64 {
        self.spec.pricing.price(&Usage {
            input_tokens,
            cached_input_tokens: 0,
            // A quote, not an observation: nothing has been sent yet.
            cache_read_source: CacheReadSource::Unreported,
            // With no measured write, pricing reserves the uncached share at
            // the configured write rate, or the input rate when no separate
            // write price exists. A cache hit must not be assumed in the grant.
            cache_write_tokens: 0,
            output_tokens: self.config.expected_output_tokens as u64,
            reasoning_tokens: 0,
            accounting: Accounting::Estimated,
        })
    }

    /// The ledger this check answers to, and the ceiling it answers under.
    ///
    /// `None` where there is nothing to ask — no ledger configured, or a
    /// membership with no budget — and both callers read it from here rather
    /// than each testing two `Option`s of their own. That is the whole reason
    /// it exists: the grant and the settle must never disagree about whether a
    /// hold was taken, and two copies of a two-way test is exactly how they
    /// would come to.
    fn payer<'a>(
        &'a self,
        side_call: &SideCall<'a>,
    ) -> Option<(&'a dyn SpendLedger, &'a BudgetTerms)> {
        Some((self.spend.as_deref()?, side_call.budget?))
    }

    /// The ledger key a check's hold and its settle share.
    ///
    /// The check's own [`SideCallId`], carried in the ledger's `ResponseId`
    /// field because that is what the ledger calls "the thing this hold belongs
    /// to". The *string* is the side call's, never a response's, so a check can
    /// no more collide with the turn it is checking than two turns can with
    /// each other — and an operator joining committed spend to the log finds
    /// the same id on both sides.
    ///
    /// [`SideCallId`]: roundhouse_core::ids::SideCallId
    fn hold_key(side_call: &SideCall<'_>) -> ResponseId {
        ResponseId::new(side_call.id.as_str())
    }

    /// The session a check's hold is recorded under.
    ///
    /// **The side call's own line, for the reason it has its own cache key**: a
    /// check is not one of the checked session's turns. The suffix is the same
    /// constant the cache isolation is named by, so the isolation is one string
    /// rather than two spellings of one idea.
    ///
    /// Not an idempotency key — a check settles under
    /// [`SettlementKey::OncePerCall`], which names the call and not a session
    /// position. See [`Self::settle`].
    fn ledger_session(side_call: &SideCall<'_>) -> SessionId {
        SessionId::new(format!("{}{VALIDATE_CACHE_SUFFIX}", side_call.session_id))
    }

    /// Reserve what this check may spend, or discover it cannot be afforded.
    ///
    /// **No fail-open.** A ledger that cannot be reached has not authorized the
    /// spend, so an unreachable ledger answers the same way an exhausted one
    /// does. The cost of being wrong in this direction is one skipped check on
    /// a turn that then proceeds untouched; the cost of being wrong the other
    /// way is a deployment spending past a ceiling an admin wrote because its
    /// ledger was briefly down.
    ///
    /// A grant of *less* than the estimate is a refusal rather than a smaller
    /// check: the prompt is already written and its price is not negotiable
    /// downwards, so a partial reservation buys nothing and is handed straight
    /// back. That release is the one settle this function owes — after it, the
    /// check has no hold and [`Self::consult`] returns before opening one.
    async fn reserve(&self, side_call: &SideCall<'_>, cost_usd: f64) -> Result<(), JudgeFailure> {
        let Some((spend, terms)) = self.payer(side_call) else {
            return Ok(());
        };
        let grant = match spend
            .open_grant(GrantRequest {
                principal: side_call.principal.clone(),
                session_id: Self::ledger_session(side_call),
                response_id: Self::hold_key(side_call),
                requested_usd: cost_usd,
                // The check's own deadline plus the same slack a turn's hold
                // carries, and both halves matter for the same reason they do
                // there: shorter and a slow check's hold lapses underneath it,
                // much longer and a dead process strands the reservation.
                ttl_ms: self.deadline_ms() + GRANT_TTL_SLACK_MS,
                terms: terms.clone(),
                now_ms: now_ms(),
            })
            .await
        {
            Ok(grant) => grant,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the spend ledger could not be reached; skipping this check rather \
                     than spending against a ceiling nobody could confirm"
                );
                return Err(JudgeFailure::Unaffordable);
            }
        };
        if grant.granted_usd < cost_usd {
            self.settle(side_call, 0.0).await;
            return Err(JudgeFailure::Unaffordable);
        }
        Ok(())
    }

    /// Close this check's hold and commit what it actually spent.
    ///
    /// Priced from the usage that came back and the card this judge is
    /// configured with — the same rule a turn's settle is under, where an
    /// estimate consumes budget exactly as a measurement would. A provider with
    /// unreliable accounting must not be able to check a session for free.
    ///
    /// **Never fails the turn.** A settle that cannot be applied is a warning
    /// and a skip, for the reason `repair_settle` is: the turn this check was
    /// made for is still running, and nothing on this path may take it down.
    /// What that costs is one check's spend left uncommitted and its hold left
    /// to lapse on the TTL — visible as ledger-versus-log drift, which is
    /// exactly where every other gap of this kind is surfaced.
    async fn settle(&self, side_call: &SideCall<'_>, actual_usd: f64) {
        let Some((spend, terms)) = self.payer(side_call) else {
            return;
        };
        if let Err(error) = spend
            .settle_grant(Settlement {
                principal: side_call.principal.clone(),
                // A check is dispatched beside the turn it is checking and
                // shares no order with the other calls under that session, so
                // its own hold is its identity. That id is minted fresh per
                // dispatch, which is what lets a check refused for budget
                // release its hold at zero without closing out a later
                // attempt's identity.
                key: SettlementKey::OncePerCall,
                response_id: Self::hold_key(side_call),
                actual_usd,
                window: terms.budget.window,
                now_ms: now_ms(),
            })
            .await
        {
            tracing::warn!(
                %error,
                side_call_id = %side_call.id,
                "a check's spend could not be committed; leaving its hold to lapse \
                 rather than failing the turn it was checking"
            );
        }
    }

    /// How long this check may take: a bounded fraction of the turn's deadline.
    fn deadline_ms(&self) -> u64 {
        (self.turn_deadline_ms as f64 * self.config.deadline_fraction) as u64
    }

    /// Drain the judge's stream into one answer, under one deadline.
    ///
    /// Every failure inside is an [`JudgeFailure::Abandoned`] against this
    /// target: the call was made, so the log gets a row for a call that
    /// produced nothing, which is what keeps a broken judge from reading as a
    /// free one.
    ///
    /// `input_tokens` is the count [`Self::counted_input_tokens`] already made
    /// for the budget question, threaded here rather than recomputed, and it is
    /// load-bearing: see the fallback at the bottom.
    async fn drain(
        &self,
        mut stream: FrontierStream,
        deadline: tokio::time::Instant,
        input_tokens: u64,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        let mut raw = String::new();
        let mut reported: Option<Usage> = None;
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(FrontierChunk::OutputText(part)))) => raw.push_str(&part),
                // **Discarded, and here that is the answer rather than a
                // deferral.** The judge's quote carries `tools: None`, so a
                // provider that produced a call did so unprompted; and what this
                // function is collecting is a *verdict* — four JSON fields it
                // parses below — not a turn to be continued. A model that
                // reached for a tool instead of answering has failed to answer,
                // which the parse then reports as an unusable verdict with the
                // raw text attached. Contrast the engine's fold, where the same
                // arm is a seam waiting on a durable item shape: a side call has
                // no such shape and needs none.
                Ok(Some(Ok(FrontierChunk::ToolCall { .. }))) => {}
                Ok(Some(Ok(FrontierChunk::Done {
                    input_tokens,
                    cached_input_tokens,
                    cache_read_source,
                    cache_write_tokens,
                    output_tokens,
                    reasoning_tokens,
                    // A judge's side call is booked from the catalog like
                    // every other dispatch. What the provider says it cost is
                    // the reconciliation view's input, not the ledger's --
                    // see `FrontierChunk::Done::provider_reported_cost`.
                    //
                    // Still discarded here after the main path learned to keep
                    // it (review finding G11), and the asymmetry is structural
                    // rather than an oversight: a side call terminates as
                    // `SideCallCompleted`, which pairs with no `Routed` and
                    // carries no settlement, so there is no terminal event with
                    // a column to put a provider figure in. Widening that event
                    // is a decision about what a side call is for, not a
                    // consequence of this fix.
                    provider_reported_cost: _,
                    // Likewise: a `SideCallCompleted` has nowhere to put one,
                    // and a judge that stopped early is caught by the verdict
                    // parse below rather than by a word from the provider.
                    stop_reason: _,
                }))) => {
                    reported = Some(Usage {
                        input_tokens,
                        cached_input_tokens,
                        cache_write_tokens,
                        output_tokens,
                        reasoning_tokens,
                        accounting: Accounting::Reported,
                        cache_read_source,
                    });
                }
                Ok(Some(Err(error))) => return Err(self.abandoned(&error)),
                Ok(None) => break,
                Err(_) => {
                    return Err(JudgeFailure::Abandoned {
                        target: self.target(),
                        reason: SideCallAbandonReason::DeadlineExceeded,
                    });
                }
            }
        }
        Ok(JudgeAnswer {
            // The judge's own accounting where it gave one, and our count of
            // what we sent and received where it did not. A side call that
            // billed nothing is indistinguishable from a free one, and this
            // deployment's own dashboard would show its checks as costless —
            // which is the direction an accounting error must never run.
            //
            // **Both axes, and the input one is the axis that matters.** A
            // check sends a multi-kilobyte brief and gets four fields back, so
            // input dominates its price; a fallback that counted only the
            // answer would book the expensive half at zero and call the
            // difference a saving. Input is not really an estimate at all — it
            // is the prompt this deployment tokenized and sent, the same number
            // the budget question was asked with — but the booking stays
            // `Estimated`, because the output beside it is a genuine guess and
            // a measurement and an estimate must never merge into one row.
            usage: reported.unwrap_or_else(|| Usage {
                input_tokens,
                cached_input_tokens: 0,
                // The provider withheld its accounting, so it withheld this.
                cache_read_source: CacheReadSource::Unreported,
                // Zero, not back-derived: a provider that withheld its
                // accounting withheld this too, and a cache-write count
                // invented here would be a pricing convention wearing the name
                // of a measurement.
                cache_write_tokens: 0,
                output_tokens: self.tokenizer.encode(&raw).len() as u64,
                reasoning_tokens: 0,
                accounting: Accounting::Estimated,
            }),
            raw,
            target: self.target(),
        })
    }

    /// Which abandon reason a provider error is, in the vocabulary an operator
    /// reads.
    ///
    /// The same three-systems split [`IncompleteReason`] is written under: a
    /// provider that answered and refused is not a provider nobody could
    /// reach, and the two send an operator to different places.
    ///
    /// [`IncompleteReason`]: roundhouse_core::event::IncompleteReason
    fn abandoned(&self, error: &FrontierError) -> JudgeFailure {
        JudgeFailure::Abandoned {
            target: self.target(),
            reason: match error {
                // A provider nobody could reach, and a credential that never
                // resolved, are the same answer to the operator: nothing was
                // asked. Grouped rather than given a fourth reason because
                // `Refused` means the provider answered, and a client that
                // declined to send an unauthenticated request has no answer to
                // report.
                // A dialect the client cannot serialize joins them: it is a
                // deployment mistake, and like the other two it means the
                // request was never sent. So does a transport failure, which is
                // the most literal reading of "nobody could reach it" the enum
                // has — it earned its own variant for failover's sake, and the
                // question this match asks is unchanged by that.
                // A quote whose segment structure does not describe its prompt
                // joins them on the same test: it is caught inside this process
                // and nothing was sent. Refusing it here rather than folding it
                // into `Refused` keeps that word meaning "the provider
                // answered", which is what an operator reads it as.
                // And a toolbox no dispatch client can restate in the resolved
                // target's dialect joins them too — unreachable in the same
                // sense, refused inside this process before a socket. It is
                // listed rather than left to a wildcard for the reason this
                // whole match is spelled out, and it is *structurally*
                // unreachable from here besides: a judge declares no tools, so
                // its quote has nothing to translate.
                FrontierError::UnknownProvider(_)
                | FrontierError::Credential(_)
                | FrontierError::MalformedQuote(_)
                | FrontierError::UntranslatableTools { .. }
                | FrontierError::UnsupportedDialect { .. }
                | FrontierError::Transport { .. } => SideCallAbandonReason::Unreachable,
                // The provider answered. A 503 and an unparseable body are both
                // an answer this deployment could not use, which is what
                // `Refused` has always meant here — a judge does not fail over,
                // so the split that matters to routing does not matter to this.
                FrontierError::Upstream(_) | FrontierError::Status { .. } => {
                    SideCallAbandonReason::Refused
                }
            },
        }
    }
}

#[async_trait]
impl<T: Tokenizer + Clone + Send + Sync + 'static> JudgeClient for FleetJudge<T> {
    async fn consult(
        &self,
        side_call: &SideCall<'_>,
        system_prompt: &str,
        brief: &str,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        // Prepared before anything is priced, and handed to the call below
        // unchanged: one string is counted, reserved against and sent.
        let prepared = PreparedPrompt::new(system_prompt, brief);
        let input_tokens = self.counted_input_tokens(&prepared);
        // The budget question first, and before any deadline is taken: a check
        // nobody can afford must cost the turn nothing at all, not a round trip
        // that is then thrown away.
        self.reserve(side_call, self.estimated_cost_usd(input_tokens))
            .await?;

        // **One exit from here down, and that is the whole of "a hold this path
        // takes is a hold it closes".** Every way this call can end — an answer,
        // a provider that refused, a deadline — meets at the settle below, so
        // there is no path on which the reservation above outlives the check it
        // was taken for. An early `?` in the body would be exactly that path.
        let answered = self.call(input_tokens, side_call, prepared).await;
        self.settle(
            side_call,
            match &answered {
                // Priced from what came back, estimate or measurement alike.
                Ok(answer) => self.spec.pricing.price(&answer.usage),
                // Nothing usable came back and nothing here can price what the
                // provider may still bill for, so the hold is released and the
                // gap is drift the reconciliation view shows. Booking a guess
                // would be inventing a number; booking the hold would charge a
                // ceiling as if it were a receipt.
                Err(_) => 0.0,
            },
        )
        .await;
        answered
    }
}

impl<T: Tokenizer + Clone> FleetJudge<T> {
    /// The call itself: quote, connect, drain — everything between the grant
    /// and the settle.
    ///
    /// Split out so that [`Self::consult`]'s money seam is two statements with
    /// one fallible expression between them, rather than a body a later `?`
    /// could quietly escape through.
    async fn call(
        &self,
        input_tokens: u64,
        side_call: &SideCall<'_>,
        prepared: PreparedPrompt,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.deadline_ms());
        let quote = FrontierQuote {
            target: self.target(),
            wire_protocol: self.spec.wire_protocol,
            // Send the same bytes that supplied the reservation's token count.
            prompt: prepared.text,
            // Messages can mark the system prefix without marking the brief.
            segment_boundaries: prepared.boundaries,
            // The conversation's stored breakpoint belongs to a different prompt.
            previous_breakpoint: None,
            // The judge and turn path use the same target TTL source.
            cache_ttl_ms: self.spec.requested_cache_ttl_ms(),
            // The isolation, and the one line of this file that would be
            // easiest to get subtly wrong: the *conversation's* key here would
            // cool the hit the router priced for the next real turn.
            session_id: None,
            thread_id: None,
            prompt_cache_key: format!("{}{VALIDATE_CACHE_SUFFIX}", side_call.session_id),
            expected_output_tokens: Some(self.config.expected_output_tokens),
            // **The judge is its own client, so it declares its own ceiling**,
            // and it is the same number as the estimate above by construction.
            // [`JudgeConfig::expected_output_tokens`]'s doc says it is used
            // twice for two things — sizing the budget question, and telling
            // the provider what to expect — and M11.1's F1 split those two uses
            // into two fields. Written out here rather than left `None` so the
            // second use survives the split unchanged: a structured verdict is
            // four fields, and a side call that inherited a dialect's generous
            // default ceiling would be a checker free to spend more than the
            // turn it is checking.
            output_token_cap: Some(self.config.expected_output_tokens),
            // **A judge is handed no tools, and that is the isolation rather
            // than a gap.** It is asked for a structured verdict about a
            // transcript, and a checker that could call the tools it is
            // reviewing would be acting inside the session it is meant to be
            // standing outside of. The tools the turn under review *declared*
            // — the toolbox's names, descriptions and schemas — never reach
            // the judge at all, inside the brief or anywhere else:
            // `ValidationBrief` (`validate/brief.rs`) is built from items,
            // hashes and sentences, and carries no field a declared toolbox
            // could arrive through — the same structural argument that module
            // makes for keeping prices and target names out. What the brief's
            // "Recent steps" section does show is narrower and different: the
            // *name* of a tool the turn actually called, plus a hash of its
            // arguments, never the schema that told the model the call was
            // available in the first place. A judge that has never seen the
            // toolbox cannot be steered by a tool description crafted to read
            // well to it.
            tools: None,
            tool_choice: None,
            // Nothing to stamp a dialect on -- see `FrontierQuote::tools_dialect`.
            tools_dialect: None,
            // **Deliberately unresolved, and this is the honest state rather
            // than an oversight.** A side call is deployment work — it is not a
            // tenant's turn and must never spend a member's key — so the only
            // tier it may draw on is the deployment's own. That tier is
            // resolved inside `ControlPlaneConfig::validate` and folded into
            // each `Admission`; handing it to a judge as well would be a second
            // reader of the same keys, held for the life of the process, and
            // deciding where that reader lives is a design question this
            // milestone did not need to answer: no named M7 test turns on it.
            //
            // What that costs is bounded and loud. A deployment that composes a
            // real provider client *and* enrols a project in the validate loop
            // gets every validation abandoned as `Unreachable`, with the
            // credential layer's own message and code — never an
            // unauthenticated request, and never a silently skipped check. The
            // gap is a missing feature, not a fail-open.
            credential: TurnCredential::Absent,
        };

        let stream = match tokio::time::timeout_at(deadline, self.client.execute(&quote)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return Err(self.abandoned(&error)),
            Err(_) => {
                return Err(JudgeFailure::Abandoned {
                    target: self.target(),
                    reason: SideCallAbandonReason::DeadlineExceeded,
                });
            }
        };
        self.drain(stream, deadline, input_tokens).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::context::ByteTokenizer;
    use roundhouse_core::control::{
        Allocation, Balance, BalanceQuery, Budget, BudgetWindow, DEFAULT_WARN_AT, Exhaustion,
        MemorySpendLedger, Principal,
    };
    use roundhouse_core::ids::{SessionId, SideCallId};
    use roundhouse_core::routing::{CacheModel, ProviderPricing};
    use roundhouse_fleet::WireProtocol;
    use std::sync::Mutex;

    /// A client that records the quote it was handed and answers from a script.
    #[derive(Default)]
    struct RecordingClient {
        seen: Mutex<Vec<FrontierQuote>>,
        fail: Option<FrontierError>,
    }

    #[async_trait]
    impl FrontierClient for RecordingClient {
        async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
            self.seen.lock().expect("recording").push(quote.clone());
            match &self.fail {
                Some(FrontierError::Upstream(message)) => {
                    Err(FrontierError::Upstream(message.clone()))
                }
                Some(FrontierError::UnknownProvider(name)) => {
                    Err(FrontierError::UnknownProvider(name.clone()))
                }
                Some(FrontierError::Credential(error)) => {
                    Err(FrontierError::Credential(error.clone()))
                }
                Some(FrontierError::MalformedQuote(why)) => {
                    Err(FrontierError::MalformedQuote(why.clone()))
                }
                Some(FrontierError::UnsupportedDialect {
                    expected,
                    got,
                    target,
                }) => Err(FrontierError::UnsupportedDialect {
                    expected,
                    got,
                    target: target.clone(),
                }),
                Some(FrontierError::UntranslatableTools { tool, from, to }) => {
                    Err(FrontierError::UntranslatableTools {
                        tool: tool.clone(),
                        from,
                        to,
                    })
                }
                Some(FrontierError::Transport { message, timed_out }) => {
                    Err(FrontierError::Transport {
                        message: message.clone(),
                        timed_out: *timed_out,
                    })
                }
                Some(FrontierError::Status { status, message }) => Err(FrontierError::Status {
                    status: *status,
                    message: message.clone(),
                }),
                None => Ok(FrontierChunk::whole_response(
                    r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#
                        .to_string(),
                    900,
                    0,
                    CacheReadSource::Provider,
                    40,
                    0,
                )),
            }
        }
    }

    /// A client that records the quote *and* answers without any accounting.
    ///
    /// One fixture rather than two because the assertions this file needs most
    /// are joins: what we counted the prompt as against what we sent, which no
    /// fixture that supplies only one half can witness.
    #[derive(Default)]
    struct RecordingSilentClient {
        seen: Mutex<Vec<FrontierQuote>>,
    }

    #[async_trait]
    impl FrontierClient for RecordingSilentClient {
        async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
            self.seen.lock().expect("recording").push(quote.clone());
            Ok(futures::stream::iter([Ok(FrontierChunk::OutputText(
                r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#
                    .to_string(),
            ))])
            .boxed())
        }
    }

    fn recording_judge(
        spec: FrontierModelSpec,
    ) -> (Arc<RecordingSilentClient>, FleetJudge<ByteTokenizer>) {
        let client = Arc::new(RecordingSilentClient::default());
        let judge = FleetJudge::new(
            Arc::clone(&client) as Arc<dyn FrontierClient>,
            spec,
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        );
        (client, judge)
    }

    /// The one quote a check put on the wire.
    async fn quote_of(
        client: &Arc<RecordingSilentClient>,
        judge: &FleetJudge<ByteTokenizer>,
        system_prompt: &str,
        brief: &str,
    ) -> FrontierQuote {
        judge
            .consult(&Check::nth(0).under(None), system_prompt, brief)
            .await
            .expect("a stream that ended is an answer");
        client.seen.lock().expect("recording").remove(0)
    }

    /// The reservation and transport use the same prompt, including the separator.
    #[tokio::test]
    async fn a_check_is_counted_on_the_bytes_it_sends() {
        let (client, judge) = recording_judge(spec());
        let answer = judge
            .consult(&Check::nth(0).under(None), "system", "brief")
            .await
            .expect("a stream that ended is an answer");
        let sent = client.seen.lock().expect("recording")[0].prompt.clone();

        assert_eq!(
            answer.usage.input_tokens,
            ByteTokenizer.encode(&sent).len() as u64,
            "the count and the transport must come off one prepared string, or \
             a check reserves for less than {sent:?}"
        );
    }

    /// The blocks a Messages client cuts are a slicing of the prompt that was
    /// counted, and they name the one stretch of a check that repeats.
    #[tokio::test]
    async fn a_check_names_its_stable_system_prefix_as_one_segment() {
        let (client, judge) = recording_judge(spec());
        let quote = quote_of(&client, &judge, "system", "brief").await;

        let segments = quote
            .segments()
            .expect("a check's boundaries describe its own prompt");
        assert_eq!(
            segments.len(),
            2,
            "the system prompt is constant across every check and the brief is \
             not, so they are the two blocks: {segments:?}"
        );
        assert_eq!(
            segments.concat(),
            quote.prompt,
            "segments are a slicing of what was sent, never a second rendering"
        );
        assert_eq!(
            (segments[0], segments[1]),
            ("system\n\n", "brief"),
            "the boundary must preserve the fixed separator and full brief: {segments:?}"
        );
    }

    /// A check asks the provider for the lifetime its own target declares,
    /// read by the rule the engine reads it by — one catalog field, so the TTL
    /// the wire asks for and the TTL a rate card is held to cannot disagree.
    #[tokio::test]
    async fn a_check_asks_for_its_targets_declared_cache_lifetime() {
        for (cache_model, requested) in [
            (CacheModel::Deterministic { ttl_ms: 300_000 }, Some(300_000)),
            (
                CacheModel::Deterministic { ttl_ms: 3_600_000 },
                Some(3_600_000),
            ),
            (CacheModel::Observed, None),
            (
                CacheModel::InactivityDecay {
                    half_life_ms: 60_000,
                    max_ttl_ms: 600_000,
                    min_prefix_tokens: 1_024,
                },
                None,
            ),
        ] {
            let (client, judge) = recording_judge(FrontierModelSpec {
                cache_model,
                ..spec()
            });
            let quote = quote_of(&client, &judge, "system", "brief").await;
            assert_eq!(quote.cache_ttl_ms, requested, "{cache_model:?}");
        }
    }

    /// **CONTROL.** The isolations a cache-aware check must not trade away: a
    /// key of its own, and no block index borrowed from the conversation's
    /// ledger, which names a block in a different prompt.
    #[tokio::test]
    async fn a_check_keeps_its_own_key_and_borrows_no_conversation_breakpoint() {
        let (client, judge) = recording_judge(spec());
        let quote = quote_of(&client, &judge, "system", "brief").await;

        assert_eq!(quote.prompt_cache_key, "acme/ada/main#validate");
        assert_eq!(
            quote.previous_breakpoint, None,
            "the conversation breakpoint does not belong to the judge prompt"
        );
        assert_eq!(
            quote.output_token_cap,
            Some(JudgeConfig::default().expected_output_tokens),
            "the checker's ceiling survives the cache work"
        );
        assert!(quote.tools.is_none());
    }

    /// An empty half names no boundary rather than one the client must refuse.
    ///
    /// A boundary at `0` or at the end of the prompt is a
    /// [`FrontierError::MalformedQuote`], so a rule that always split would
    /// turn an empty brief into an abandoned check.
    #[tokio::test]
    async fn an_empty_prompt_half_names_no_boundary() {
        for (system_prompt, brief, boundaries) in [
            ("system", "brief", 1),
            ("", "brief", 0),
            ("system", "", 0),
            ("", "", 0),
        ] {
            let (client, judge) = recording_judge(spec());
            let quote = quote_of(&client, &judge, system_prompt, brief).await;
            assert_eq!(
                quote.segment_boundaries.len(),
                boundaries,
                "({system_prompt:?}, {brief:?})"
            );
            let segments = quote
                .segments()
                .unwrap_or_else(|error| panic!("({system_prompt:?}, {brief:?}): {error}"));
            assert_eq!(segments.concat(), quote.prompt);
        }
    }

    /// **CONTROL.** A check marks a block for caching, so what it reserves is
    /// the cold write and never the plain input rate.
    ///
    /// The direction is the whole point: reserving at the input rate and then
    /// asking the provider for a charged write is a grant that cannot settle.
    /// [`ProviderPricing::price`] takes the conservative branch when nothing
    /// measured a write, which is what makes this hold — and what a second,
    /// judge-local pricing rule would quietly undo.
    #[tokio::test]
    async fn a_check_reserves_the_write_premium_and_refuses_before_it_sends() {
        let ledger = Arc::new(MemorySpendLedger::new());
        let client = Arc::new(RecordingSilentClient::default());
        let judge = FleetJudge::new(
            Arc::clone(&client) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        )
        .with_spend_ledger(Arc::clone(&ledger) as Arc<dyn SpendLedger>);

        let brief = "x".repeat(4_000);
        let tokens = ByteTokenizer.encode(&format!("system\n\n{brief}")).len() as f64;
        let output = JudgeConfig::default().expected_output_tokens as f64;
        let card = spec().pricing;
        let per_mtok = 1e-6;
        let answer = output * card.output_per_mtok_usd * per_mtok;
        let input_only = tokens * card.input_per_mtok_usd * per_mtok + answer;
        let cold_write = tokens * card.cache_write_per_mtok_usd * per_mtok + answer;
        assert!(
            input_only < cold_write,
            "the fixture card must price a write above plain input, or this \
             asserts nothing"
        );

        let between = terms((input_only + cold_write) / 2.0);
        assert_eq!(
            judge
                .consult(&Check::nth(0).under(Some(&between)), "system", &brief)
                .await,
            Err(JudgeFailure::Unaffordable),
            "a ceiling that covers the prompt at the input rate but not at the \
             write premium must refuse the check"
        );
        assert!(
            client.seen.lock().expect("recording").is_empty(),
            "and refuse it before the socket, not after"
        );

        // The control: a ceiling that covers the premium makes the same check.
        let funded = terms(cold_write * 2.0);
        judge
            .consult(&Check::nth(1).under(Some(&funded)), "system", &brief)
            .await
            .expect("a funded membership gets its check");
        assert_eq!(client.seen.lock().expect("recording").len(), 1);
    }

    /// **CONTROL.** A card that prices no separate write bills the plain input
    /// rate. A dialect with no write accounting must not be charged a premium
    /// nobody published.
    #[test]
    fn a_target_that_prices_no_write_reserves_the_plain_input_rate() {
        let spec = FrontierModelSpec {
            pricing: ProviderPricing {
                cache_write_per_mtok_usd: 0.0,
                ..spec().pricing
            },
            ..spec()
        };
        let judge = FleetJudge::new(
            Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
            spec.clone(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        );
        let tokens = 1_000u64;
        let expected = tokens as f64 * spec.pricing.input_per_mtok_usd * 1e-6
            + JudgeConfig::default().expected_output_tokens as f64
                * spec.pricing.output_per_mtok_usd
                * 1e-6;

        assert!(
            (judge.estimated_cost_usd(tokens) - expected).abs() < 1e-12,
            "{} against {expected}",
            judge.estimated_cost_usd(tokens)
        );
    }

    /// A provider that streams an answer and never says what it billed.
    ///
    /// The common case rather than an anomaly — a streaming OpenAI-compatible
    /// endpoint sends no usage unless the request asked for it, and a gateway
    /// in the path can drop it even when it did — and the one the judge's own
    /// accounting has to fill in rather than book as free.
    struct SilentClient;

    #[async_trait]
    impl FrontierClient for SilentClient {
        async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
            Ok(futures::stream::iter([Ok(FrontierChunk::OutputText(
                r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#
                    .to_string(),
            ))])
            .boxed())
        }
    }

    fn spec() -> FrontierModelSpec {
        FrontierModelSpec {
            provider: "anthropic".into(),
            model: "claude".into(),
            wire_protocol: WireProtocol::AnthropicMessages,
            cache_model: CacheModel::Deterministic { ttl_ms: 300_000 },
            pricing: ProviderPricing {
                input_per_mtok_usd: 3.0,
                cached_input_per_mtok_usd: 0.3,
                cache_write_per_mtok_usd: 3.75,
                output_per_mtok_usd: 15.0,
            },
            quality_prior: 0.95,
            base_ttft_ms: 350.0,
            ttft_ms_per_uncached_token: 0.002,
        }
    }

    fn terms(limit_usd: f64) -> BudgetTerms {
        BudgetTerms {
            budget: Budget {
                limit_usd,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: DEFAULT_WARN_AT,
            },
            allocation: Allocation::Pooled,
        }
    }

    /// What one validated turn of a session hands the judge, owned so a test
    /// can lend it out.
    ///
    /// `n` is which validated turn of the session this is, and it moves the one
    /// field a repeat has to move: a fresh id, so two checks can neither share a
    /// hold nor have the second's settle deduplicated as a repeat of the first.
    /// A fixture that reused one `Check` across several consults would be
    /// modelling a single turn checked many times, which no engine does — and
    /// would now be silently dropping every settle after the first.
    struct Check {
        session_id: SessionId,
        principal: Principal,
        id: SideCallId,
    }

    impl Check {
        fn nth(n: u64) -> Self {
            Self {
                session_id: SessionId::new("acme/ada/main"),
                principal: Principal::new("acme", "ada"),
                id: SideCallId::new(format!("sc_{n}")),
            }
        }

        fn under<'a>(&'a self, budget: Option<&'a BudgetTerms>) -> SideCall<'a> {
            SideCall {
                session_id: &self.session_id,
                id: &self.id,
                principal: &self.principal,
                budget,
            }
        }
    }

    #[tokio::test]
    async fn the_side_call_carries_its_own_cache_key_and_never_the_conversations() {
        let client = Arc::new(RecordingClient::default());
        let judge = FleetJudge::new(
            Arc::clone(&client) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        );
        let first = Check::nth(0);
        let second = Check::nth(1);

        judge
            .consult(&first.under(None), "system", "brief")
            .await
            .expect("the scripted client answers");
        // And again: the key is stable across validations, which is what lets
        // the judge's own prefix warm.
        judge
            .consult(&second.under(None), "system", "a later brief")
            .await
            .expect("the scripted client answers");

        let seen = client.seen.lock().expect("recording");
        let keys: Vec<&str> = seen
            .iter()
            .map(|quote| quote.prompt_cache_key.as_str())
            .collect();
        assert_eq!(keys, ["acme/ada/main#validate", "acme/ada/main#validate"]);
        // The control that makes the assertion above about isolation rather
        // than about a string: the conversation's own key is what the engine
        // sends, and it must not be what this sent.
        assert!(
            keys.iter().all(|key| *key != first.session_id.to_string()),
            "a judge prompt on the conversation's key cools the hit the router \
             just priced: {keys:?}"
        );
    }

    #[tokio::test]
    async fn a_budget_with_no_room_skips_the_check_instead_of_failing_the_turn() {
        let client = Arc::new(RecordingClient::default());
        let ledger = Arc::new(MemorySpendLedger::new());
        let judge = FleetJudge::new(
            Arc::clone(&client) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        )
        .with_spend_ledger(Arc::clone(&ledger) as Arc<dyn SpendLedger>);

        // A limit far below the price of one check: 400 bytes of prompt on this
        // card is dollars, and the ceiling is a fraction of a cent.
        let brief = "x".repeat(400);
        let broke = terms(0.000_001);
        let refused = judge
            .consult(&Check::nth(0).under(Some(&broke)), "system", &brief)
            .await;
        assert_eq!(refused, Err(JudgeFailure::Unaffordable));
        assert!(
            client.seen.lock().expect("recording").is_empty(),
            "a check nobody can afford must cost the turn nothing at all, not a \
             round trip that is then thrown away"
        );
        assert_eq!(
            position(&ledger, &broke).await.held_usd,
            0.0,
            "and it must leave nothing behind: a refusal that stranded the \
             partial reservation it was refused on would tighten the ceiling \
             again on the next turn"
        );

        // The control: the identical check under a ceiling that covers it is
        // made, so the refusal above is about the budget and not about the
        // fixture.
        let funded = terms(100.0);
        judge
            .consult(&Check::nth(1).under(Some(&funded)), "system", &brief)
            .await
            .expect("a funded membership gets its check");
        assert_eq!(client.seen.lock().expect("recording").len(), 1);
    }

    #[tokio::test]
    async fn a_provider_that_refuses_is_abandoned_against_its_own_target() {
        let client = Arc::new(RecordingClient {
            seen: Mutex::new(Vec::new()),
            fail: Some(FrontierError::Upstream("429".into())),
        });
        let judge = FleetJudge::new(
            client as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        );

        let failed = judge
            .consult(&Check::nth(0).under(None), "system", "brief")
            .await;
        assert_eq!(
            failed,
            Err(JudgeFailure::Abandoned {
                target: spec().target(),
                reason: SideCallAbandonReason::Refused,
            }),
            "a provider that answered and refused is not a provider nobody \
             could reach, and the two send an operator to different places"
        );
    }

    /// What the [`RecordingClient`] fixture's reported usage costs on
    /// [`spec`]'s card: 900 input and 40 output tokens.
    fn recorded_cost_usd() -> f64 {
        spec().pricing.price(&Usage {
            input_tokens: 900,
            cached_input_tokens: 0,
            cache_read_source: CacheReadSource::Provider,
            // `whole_response` reports none, which is what an adapted
            // non-streaming backend knows about a remote cache write.
            cache_write_tokens: 0,
            output_tokens: 40,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        })
    }

    async fn position(ledger: &Arc<MemorySpendLedger>, terms: &BudgetTerms) -> Balance {
        ledger
            .balance(BalanceQuery {
                principal: Principal::new("acme", "ada"),
                terms: terms.clone(),
                now_ms: now_ms(),
            })
            .await
            .expect("the memory ledger answers")
    }

    fn judge_over(
        client: Arc<dyn FrontierClient>,
        ledger: &Arc<MemorySpendLedger>,
    ) -> FleetJudge<ByteTokenizer> {
        FleetJudge::new(
            client,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        )
        .with_spend_ledger(Arc::clone(ledger) as Arc<dyn SpendLedger>)
    }

    /// **What a check costs reaches the ledger, or no ceiling bounds it.**
    ///
    /// The judge's dollars were folded into metrics and reported on the wire,
    /// and committed nowhere: the only settle in the system prices a *turn's*
    /// terminal event, and a side call is a separate model call with no
    /// terminal event of its own. So `measured_usd` moved and `committed_usd`
    /// did not, and the pre-flight budget read — which asks about one check at
    /// a time — could never see the spend of the checks before it.
    #[tokio::test]
    async fn what_a_check_spends_is_committed_to_the_payers_ledger() {
        let ledger = Arc::new(MemorySpendLedger::new());
        let judge = judge_over(Arc::new(RecordingClient::default()), &ledger);
        let terms = terms(100.0);

        judge
            .consult(&Check::nth(0).under(Some(&terms)), "system", "brief")
            .await
            .expect("a funded membership gets its check");

        let after = position(&ledger, &terms).await;
        assert!(
            (after.committed_usd - recorded_cost_usd()).abs() < 1e-12,
            "the check's own reported usage, priced on the judge's card, is what \
             the ledger must hold: {after:?} against {}",
            recorded_cost_usd()
        );
        assert_eq!(
            after.held_usd, 0.0,
            "the hold is closed by the settle, not left to lapse on a TTL — a \
             check that stranded a reservation every validation would be worse \
             than the overspend it prevents"
        );
    }

    /// The consequence, and the assertion the whole finding is about: once a
    /// membership's checks have spent its ceiling, the next check is refused.
    #[tokio::test]
    async fn checks_stop_once_their_own_spend_has_reached_the_ceiling() {
        let ledger = Arc::new(MemorySpendLedger::new());
        let judge = judge_over(Arc::new(RecordingClient::default()), &ledger);
        // A ceiling a few checks wide: each one bills ~$0.0033, and each one's
        // *estimate* is under a cent on its own, so nothing but committed spend
        // can stop the tenth.
        let ceiling = terms(0.01);

        let mut allowed = 0;
        for turn in 0..10 {
            if judge
                .consult(&Check::nth(turn).under(Some(&ceiling)), "system", "brief")
                .await
                .is_ok()
            {
                allowed += 1;
            }
        }
        assert!(
            (1..10).contains(&allowed),
            "a $0.01 ceiling must stop granting checks once real judge spend has \
             exceeded it, but {allowed} of 10 checks were allowed"
        );
        let after = position(&ledger, &ceiling).await;
        assert!(
            after.project_remaining_usd < recorded_cost_usd(),
            "and it must be the ceiling that stopped them: {after:?}"
        );

        // The control: the identical run under a ceiling that covers it makes
        // every check, so the refusals above are about the money and not about
        // the fixture running out of scripted answers.
        let roomy = terms(100.0);
        let funded = judge_over(
            Arc::new(RecordingClient::default()),
            &Arc::new(MemorySpendLedger::new()),
        );
        for turn in 0..10 {
            funded
                .consult(&Check::nth(turn).under(Some(&roomy)), "system", "brief")
                .await
                .expect("a funded membership is checked every time");
        }
    }

    /// A check that was made and produced nothing must not hold money either.
    #[tokio::test]
    async fn an_abandoned_check_gives_its_hold_back() {
        let ledger = Arc::new(MemorySpendLedger::new());
        let judge = judge_over(
            Arc::new(RecordingClient {
                seen: Mutex::new(Vec::new()),
                fail: Some(FrontierError::Upstream("429".into())),
            }),
            &ledger,
        );
        let funded = terms(100.0);

        let failed = judge
            .consult(&Check::nth(0).under(Some(&funded)), "system", "brief")
            .await;
        assert!(matches!(failed, Err(JudgeFailure::Abandoned { .. })));

        let after = position(&ledger, &funded).await;
        assert_eq!(
            after.held_usd, 0.0,
            "a judge that is refusing every call would otherwise strand a hold \
             per turn for a TTL, which is the failure a hold on this path was \
             once rejected for: {after:?}"
        );
        assert_eq!(
            after.committed_usd, 0.0,
            "and nothing is booked, because nothing this deployment can price \
             was produced"
        );

        // The half that makes both assertions above about *releasing* rather
        // than about never holding at all: a second check under a ceiling one
        // and a half checks wide is still made. Had the abandoned call kept its
        // reservation, half a check's room would be left and this would come
        // back `Unaffordable` — a judge that is refusing every call would
        // tighten its own budget one dead check at a time.
        let estimate = judge.estimated_cost_usd(
            judge.counted_input_tokens(&PreparedPrompt::new("system", "brief")),
        );
        let narrow = terms(estimate * 1.5);
        assert!(
            matches!(
                judge
                    .consult(&Check::nth(1).under(Some(&narrow)), "system", "brief")
                    .await,
                Err(JudgeFailure::Abandoned { .. })
            ),
            "the second check must reach the provider and fail there, not be \
             refused by money the first check never gave back: {:?}",
            position(&ledger, &narrow).await
        );
    }

    /// A stream that ends without an accounting chunk is booked at what we
    /// sent, never at zero.
    ///
    /// The input axis dominates a check's cost — a multi-kilobyte brief against
    /// a four-field verdict — and it is the one axis the fallback used to
    /// hardcode to zero, while the estimate the budget question was asked with,
    /// three functions up the same file, already counted it exactly.
    #[tokio::test]
    async fn a_check_nobody_billed_for_is_estimated_from_what_we_sent() {
        let judge = FleetJudge::new(
            Arc::new(SilentClient) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        );
        let system_prompt = "system";
        // A brief that dwarfs the verdict, which is the realistic shape and the
        // reason a zero on this axis is not a rounding error.
        let brief = "x".repeat(4_000);

        let answer = judge
            .consult(&Check::nth(0).under(None), system_prompt, &brief)
            .await
            .expect("a stream that ended is an answer, not a failure");

        assert_eq!(
            answer.usage.input_tokens,
            ByteTokenizer
                .encode(&PreparedPrompt::new(system_prompt, &brief).text)
                .len() as u64,
            "the prompt is what we tokenized and sent, so it is a count and not \
             a guess — the joined string, because that is what went out"
        );
        assert!(answer.usage.output_tokens > 0);
        assert_eq!(
            answer.usage.accounting,
            Accounting::Estimated,
            "measured and estimated never merge: a filled-in gap must stay \
             distinguishable from a provider's own number"
        );

        // The control: the identical call over a provider that *does* account
        // for itself carries the provider's numbers, stamped as reported.
        let reporting = FleetJudge::new(
            Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig::default(),
        );
        let reported = reporting
            .consult(&Check::nth(1).under(None), system_prompt, &brief)
            .await
            .expect("the scripted client answers");
        assert_eq!(reported.usage.accounting, Accounting::Reported);
        assert_eq!(
            (reported.usage.input_tokens, reported.usage.output_tokens),
            (900, 40)
        );
    }

    #[test]
    fn a_deadline_fraction_at_or_past_one_is_clamped_below_the_turns() {
        let judge = FleetJudge::new(
            Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig {
                deadline_fraction: 4.0,
                ..JudgeConfig::default()
            },
        );
        assert!(
            judge.config.deadline_fraction < 1.0,
            "the checker must never break the checked, and a fraction is the \
             only thing standing between a hung judge and the turn's whole budget"
        );
        // The control: a sane fraction is left exactly as written.
        let judge = FleetJudge::new(
            Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
            spec(),
            ByteTokenizer,
            120_000,
            JudgeConfig {
                deadline_fraction: 0.25,
                ..JudgeConfig::default()
            },
        );
        assert_eq!(judge.config.deadline_fraction, 0.25);
    }
}
