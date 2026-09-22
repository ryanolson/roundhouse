// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The policy boundary in front of the TypeSafe transport: who may be asked,
//! with how much, and on whose budget.
//!
//! `roundhouse_fleet::typesafe` sends a `state` string and validates what comes
//! back. Everything that decides whether that call may happen at all is here:
//! the opt-in, the admitted-frontier condition, the bounded projection, and the
//! grant-and-settle against a separate evaluation ledger.
//!
//! **Deliberately unwired.** Nothing constructs a [`TypeSafeShadow`] in any
//! shipped binary, and there is no configuration loader or startup path that
//! would. `PLAN-routing-strategy-bandit.md` puts the two missing halves in B2 —
//! the durable allocation record that names a call, its segment identity and
//! its selection probability — and B3 — the bounded background executor that
//! makes one, with cancellation and duplicate-delivery handling. Neither exists
//! yet, so no scheduler, no strategy selection and no reward accounting live
//! here. A passing test in this module does not authorize serving allocation.
//!
//! ## Why the entry point takes items rather than a brief
//!
//! A `&ValidationBrief` is not proof of boundedness.
//! [`BriefConfig`](roundhouse_core::validate::BriefConfig) bounds the
//! instructions, the step count and each output head — and nothing else. A
//! step's tool name is cloned from the transcript verbatim, the facts vector is
//! passed straight through, and a declared objective's `plan_steps` are each
//! truncated while their *count* is not. All three are unbounded input on a
//! path that is about to pay per token, so this module builds the brief itself
//! under [`ShadowCaps`] and never accepts one.
//!
//! The final rendered payload is then **rejected** rather than cut. Cutting it
//! would slice markdown the brief's quoting discipline depends on, and a
//! half-quoted hostile tool result is exactly the forgery
//! `validate::brief` exists to prevent.
//!
//! ## Three accounting outcomes
//!
//! A spend that was reported is settled at what it cost. A spend that was *not*
//! reported settles at zero — that is how the hold is released, the same
//! discipline `judge.rs` is under — but it is recorded as
//! [`Accounting::Unknown`], never as a measured zero. Releasing a hold and
//! claiming a free call are different statements, and only the first is true.

use std::collections::BTreeMap;
use std::sync::Arc;

use roundhouse_core::context::Tokenizer;
use roundhouse_core::control::{
    BudgetTerms, GrantRequest, Principal, Settlement, SettlementKey, SpendLedger, TurnCredential,
};
use roundhouse_core::event::Usage;
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::Admitted;
use roundhouse_core::routing::ledger::ProviderPricing;
use roundhouse_core::validate::{BriefConfig, Objective, ValidationBrief, truncate};
use roundhouse_fleet::typesafe::{
    ChoiceAnswer, ChoiceQuestion, PreparedRequest, SignalError, SystemOneClient, SystemOneError,
    SystemOneRequest, SystemOneUsage,
};

use crate::engine::spend::GRANT_TTL_SLACK_MS;

/// Shared by request construction and answer lookup so the keys cannot drift.
pub const TIER_KEY: &str = "tier";

/// The bounds this module puts on a brief, over and above [`BriefConfig`].
///
/// One type per field `BriefConfig` does not reach. No [`Default`]: a
/// deployment that has not chosen them has not decided how much of a
/// transcript it is willing to send to a third party.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowCaps {
    pub brief: BriefConfig,
    /// A step's tool name comes from the transcript verbatim.
    pub max_tool_name_chars: usize,
    pub max_facts: usize,
    pub max_fact_chars: usize,
    /// A declared objective's plan is a list the agent controls the length of.
    pub max_plan_steps: usize,
    /// The whole rendered state. Over this, the call does not happen.
    ///
    /// `SystemOneLimits::max_request_bytes` separately bounds serialized JSON,
    /// including escaping, the model ID, and the question. Passing this state
    /// limit does not guarantee that the complete request fits its wire limit.
    pub max_state_bytes: usize,
}

/// What one deployment decided about shadow classification.
///
/// No [`Default`], and `enabled` is private: the only way to get an enabled
/// config is to name a model, a rate card and every cap, and then say so.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowConfig {
    enabled: bool,
    /// The pinned model id. Never defaulted — `jev-latest` re-ranks itself
    /// underneath a deployment that never changed a line.
    pub model: String,
    /// Configuration, not measurement. Rate cards never go in source.
    pub pricing: ProviderPricing,
    /// What the estimate is asked with. This service answers a structured
    /// choice, so it is small on purpose.
    pub expected_output_tokens: u64,
    pub caps: ShadowCaps,
}

impl ShadowConfig {
    /// Disabled, whatever else is configured.
    pub fn new(
        model: impl Into<String>,
        pricing: ProviderPricing,
        expected_output_tokens: u64,
        caps: ShadowCaps,
    ) -> Self {
        Self {
            enabled: false,
            model: model.into(),
            pricing,
            expected_output_tokens,
            caps,
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
}

/// Why no call was made. Every arm is a refusal taken before any HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRun {
    /// No deployment opted in. The default.
    Disabled,
    /// Nothing frontier survived admission, so this session's content has no
    /// third party it is already permitted to reach. Conservative on purpose:
    /// a local-only session makes zero calls.
    NoAdmittedFrontier,
    /// The bounded projection still did not fit. Rejected, never cut.
    PayloadTooLarge {
        limit_bytes: usize,
        actual_bytes: usize,
    },
    /// The transport would refuse this request before a socket: a credential
    /// that is not this deployment's to spend, or a body over the wire bound.
    ///
    /// Decided *before* the grant. A refusal taken afterwards would open and
    /// close a hold for a call that was never eligible, and would report a
    /// failed call for a condition in which nothing was ever sent.
    Refused(SystemOneError),
    /// The evaluation budget granted less than the call was estimated at. The
    /// hold, if one was taken, has been released.
    BudgetRefused,
    /// The ledger could not be reached. Fail closed: a call spent against a
    /// ceiling nobody could confirm is an unbudgeted call.
    LedgerUnavailable,
}

/// What a call cost, or that nobody can say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Accounting {
    /// Priced from the usage the service reported, and submitted for
    /// settlement.
    ///
    /// **Not a guarantee that it was committed.** A settle that cannot be
    /// applied is warned and skipped rather than propagated, so this says what
    /// the configured-rate cost submitted to the ledger, not what it accepted.
    /// The warning identifies the session and hold. Durable evaluation records
    /// remain part of B2 in `PLAN-routing-strategy-bandit.md`.
    Measured { usage: SystemOneUsage, usd: f64 },
    /// Usage was absent or incomplete. Releasing the hold at zero does not
    /// establish that the call was free.
    Unknown,
}

/// What one shadow classification produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowOutcome {
    NotRun(NotRun),
    Answered {
        answer: ChoiceAnswer,
        accounting: Accounting,
    },
    /// An answer arrived and could not be used. Its accounting survives, which
    /// is the whole reason the transport keeps usage beside the signal.
    Unusable {
        signal: SignalError,
        accounting: Accounting,
    },
    /// No envelope came back, so there is nothing to price and no `Accounting`
    /// to carry. The hold is released at zero; this is unknown accounting by
    /// construction rather than a free call.
    Failed {
        error: SystemOneError,
    },
}

/// The durable identity a call is held and settled under.
///
/// Supplied by the caller rather than minted here: B2 owns the allocation
/// record these have to agree with, and a module that invented its own would be
/// a second answer to a question that record already answers.
pub struct ShadowCall<'a> {
    pub principal: Principal,
    pub session_id: SessionId,
    /// The ledger key this call's hold is taken under, and the identity its
    /// settle is deduplicated by. Distinct from any turn's, and distinct per
    /// attempt: a settled identity can never settle again.
    pub hold_key: ResponseId,
    pub terms: BudgetTerms,
    pub credential: &'a TurnCredential,
    pub now_ms: u64,
}

/// The policy boundary in front of one System One upstream.
pub struct TypeSafeShadow<T: Tokenizer> {
    client: SystemOneClient,
    config: ShadowConfig,
    /// Required — there is no unbudgeted path through this type.
    ///
    /// Whether this is a *separate* ledger from the project's is the caller's
    /// to arrange and cannot be checked here: a `SpendLedger` handed in is
    /// indistinguishable from the turn's. What the design asks for is an
    /// evaluation ledger with its own ceiling; what this field enforces is
    /// only that some ledger was supplied.
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

    /// The questions this module asks: one, under [`TIER_KEY`].
    ///
    /// This adapter retains its tier question. Rich turn classifications remain
    /// separate work in `PLAN-routing-strategy-bandit.md`.
    ///
    /// Two options and no model names. The labels are *tiers*, so the answer
    /// maps to an admitted target through the existing routing code rather than
    /// naming one — the same rule `validate::brief` holds: the judge answers a
    /// task question and code takes the routing decision.
    pub fn questions() -> BTreeMap<String, ChoiceQuestion> {
        BTreeMap::from([(
            TIER_KEY.to_string(),
            ChoiceQuestion {
                instructions: "Which kind of model should answer this?".into(),
                criteria: [
                    (
                        "capable".to_string(),
                        "Hard, multi-step work where a mistake is expensive".to_string(),
                    ),
                    (
                        "efficient".to_string(),
                        "Routine work whose result is cheap to check".to_string(),
                    ),
                ]
                .into_iter()
                .collect(),
            },
        )])
    }

    /// The bounded projection, rendered, or why it cannot be sent.
    pub fn state(
        &self,
        items: &[Item],
        objective: Objective,
        facts: Vec<String>,
    ) -> Result<String, NotRun> {
        let caps = &self.config.caps;
        // The three axes `BriefConfig` does not reach, bounded before the brief
        // is built rather than after, so nothing unbounded is ever rendered.
        let objective = match objective {
            Objective::Declared {
                goal,
                mut plan_steps,
                done_when,
            } => {
                plan_steps.truncate(caps.max_plan_steps);
                Objective::Declared {
                    goal,
                    plan_steps,
                    done_when,
                }
            }
            other => other,
        };
        let mut facts = facts;
        facts.truncate(caps.max_facts);
        let facts = facts
            .iter()
            .map(|fact| truncate(fact, caps.max_fact_chars))
            .collect();

        let mut brief = ValidationBrief::build(items, objective, facts, caps.brief);
        for step in &mut brief.steps {
            step.name = truncate(&step.name, caps.max_tool_name_chars);
        }

        // Rejected rather than cut. Slicing the rendered markdown would cut the
        // line-prefix quoting that contains a hostile transcript, and a
        // half-quoted tool result is the forgery `validate::brief` exists to
        // prevent.
        let rendered = brief.render();
        if rendered.len() > caps.max_state_bytes {
            return Err(NotRun::PayloadTooLarge {
                limit_bytes: caps.max_state_bytes,
                actual_bytes: rendered.len(),
            });
        }
        Ok(rendered)
    }

    /// Classify one session's state, if every gate allows it.
    pub async fn classify(
        &self,
        call: ShadowCall<'_>,
        items: &[Item],
        objective: Objective,
        facts: Vec<String>,
        admitted: &Admitted<'_>,
    ) -> ShadowOutcome {
        if !self.config.enabled {
            return ShadowOutcome::NotRun(NotRun::Disabled);
        }
        // The caller supplies this admitted pool after credential and tool
        // filtering. A catalog identity alone does not establish admission.
        // Requiring an admitted frontier target conservatively excludes
        // local-only sessions from hosted classification.
        if !admitted
            .pool()
            .iter()
            .any(|candidate| !candidate.target.is_local())
        {
            return ShadowOutcome::NotRun(NotRun::NoAdmittedFrontier);
        }

        let state = match self.state(items, objective, facts) {
            Ok(state) => state,
            Err(refusal) => return ShadowOutcome::NotRun(refusal),
        };

        let request = SystemOneRequest {
            model: self.config.model.clone(),
            state,
            questions: Self::questions(),
        };
        // Before the grant, not after. Everything the transport can refuse
        // without a socket is an eligibility question, and answering it with a
        // hold already open charges the experiment's ledger a round trip for a
        // call that was never going to happen.
        let prepared = match self.client.prepare(&request, call.credential) {
            Ok(prepared) => prepared,
            Err(error) => return ShadowOutcome::NotRun(NotRun::Refused(error)),
        };

        let estimate = self.estimated_cost_usd(&prepared);
        match self.reserve(&call, estimate).await {
            Ok(()) => {}
            Err(refusal) => return ShadowOutcome::NotRun(refusal),
        }

        match self.client.send(prepared).await {
            Err(error) => {
                // Nothing priceable came back. The hold is handed back at zero
                // and the outcome says so by carrying no accounting at all.
                self.settle(&call, 0.0).await;
                ShadowOutcome::Failed { error }
            }
            Ok(reply) => {
                // Two statements, kept apart on purpose. The ledger is sent
                // zero when nothing priceable came back, because that is how a
                // hold is released; the *outcome* says `Unknown`, because a
                // measured zero would book a billed call as free.
                let accounting = match reply.usage {
                    Some(usage) => Accounting::Measured {
                        usage,
                        usd: self.price(usage),
                    },
                    None => Accounting::Unknown,
                };
                self.settle(
                    &call,
                    match accounting {
                        Accounting::Measured { usd, .. } => usd,
                        Accounting::Unknown => 0.0,
                    },
                )
                .await;
                match reply.answers {
                    // `remove` rather than a borrow-and-clone: this is the last
                    // read of the batch, and the answer is owned from here on.
                    Ok(mut answers) => match answers.remove(TIER_KEY) {
                        Some(answer) => ShadowOutcome::Answered { answer, accounting },
                        // The transport checks this key; retain a non-panicking
                        // failure if that contract changes.
                        None => ShadowOutcome::Unusable {
                            signal: SignalError::MissingAnswer,
                            accounting,
                        },
                    },
                    Err(signal) => ShadowOutcome::Unusable { signal, accounting },
                }
            }
        }
    }

    /// Estimate from the complete serialized request and expected output.
    /// Including the framing avoids omitting bytes that reach the service.
    /// This deployment's tokenizer need not match the service's tokenizer,
    /// so the estimate is not a guaranteed upper bound on billed tokens.
    /// Settlement uses reported usage instead.
    fn estimated_cost_usd(&self, prepared: &PreparedRequest) -> f64 {
        self.price(SystemOneUsage {
            input_tokens: self.tokenizer.encode(prepared.body()).len() as u64,
            output_tokens: self.config.expected_output_tokens,
        })
    }

    /// Price two axes through the one definition of what a call costs.
    ///
    /// `cached_input_tokens` is zero because this service reports no cache
    /// term — an absent measurement, not a claim that nothing was cached.
    fn price(&self, usage: SystemOneUsage) -> f64 {
        self.config.pricing.price(&Usage {
            input_tokens: usage.input_tokens,
            cached_input_tokens: 0,
            output_tokens: usage.output_tokens,
            ..Default::default()
        })
    }

    /// Hold the estimate, or refuse.
    async fn reserve(&self, call: &ShadowCall<'_>, cost_usd: f64) -> Result<(), NotRun> {
        let grant = match self
            .spend
            .open_grant(GrantRequest {
                principal: call.principal.clone(),
                session_id: call.session_id.clone(),
                response_id: call.hold_key.clone(),
                requested_usd: cost_usd,
                ttl_ms: self.client.limits().deadline_ms + GRANT_TTL_SLACK_MS,
                terms: call.terms.clone(),
                now_ms: call.now_ms,
            })
            .await
        {
            Ok(grant) => grant,
            // Fail closed. A ledger that cannot be reached has not said there
            // is room, and a call made anyway is an unbudgeted one.
            Err(_) => return Err(NotRun::LedgerUnavailable),
        };
        // **A zero grant is an ordinary ledger answer, not an error**, so a
        // refusal spelled as `open_grant(..).is_err()` would send this request
        // unfunded against an exhausted budget. A grant of *less* than the
        // estimate is equally a refusal: the prompt is already written and its
        // price is not negotiable downwards, so a partial reservation buys
        // nothing and is handed straight back rather than left to lapse.
        if grant.granted_usd < cost_usd {
            self.settle(call, 0.0).await;
            return Err(NotRun::BudgetRefused);
        }
        Ok(())
    }

    /// Release this call's hold and commit what it actually spent.
    ///
    /// Never propagates: a settle that cannot be applied is a warning and a
    /// skip, the same rule `judge.rs` is under. What that costs is one call's
    /// spend uncommitted and its hold left to lapse on the TTL.
    ///
    /// The warning identifies the session and hold so an operator can locate
    /// the failed settlement. It excludes the transcript and credential.
    async fn settle(&self, call: &ShadowCall<'_>, actual_usd: f64) {
        if let Err(error) = self
            .spend
            .settle_grant(Settlement {
                principal: call.principal.clone(),
                // **Not the session's watermark.** Several of these are in
                // flight under one session and finish in whatever order their
                // upstreams answer, so a settle keyed by log position would
                // read the call that finished last but was issued first as a
                // replay — dropping its charge and stranding its hold.
                key: SettlementKey::OncePerCall,
                response_id: call.hold_key.clone(),
                actual_usd,
                window: call.terms.budget.window,
                now_ms: call.now_ms,
            })
            .await
        {
            tracing::warn!(
                %error,
                session_id = %call.session_id,
                hold_key = %call.hold_key,
                "a shadow evaluation's spend could not be committed; leaving its \
                 hold to lapse rather than retrying against an unknown ceiling"
            );
        }
    }
}

#[cfg(test)]
mod tests;
