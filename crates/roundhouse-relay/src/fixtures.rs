// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Session logs to produce documents from.
//!
//! One builder for the whole crate, not one per module. Three producers read the
//! same replay, and the claims worth testing are about the *three agreeing* —
//! that the ATOF stream, the trajectory and the summaries describe one session.
//! Two builders would be two clocks, and a test comparing an ATOF timestamp
//! against a trajectory's would be asserting about the fixtures.
//!
//! The clock is a counter: `at_ms` advances by a fixed step per event and `seq`
//! is contiguous, exactly as the store assigns them. Nothing here reads a real
//! clock, so every expected value in every test is a literal rather than a
//! recomputation of the fixture's own arithmetic.

use roundhouse_core::control::{Billing, BudgetState, Payer, Principal};
use roundhouse_core::event::{Accounting, IncompleteReason, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{Candidate, DecisionRecord, ProviderPricing, Target};

/// The rate card every fixture prices against.
pub const HOSTED: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

/// The first event's timestamp, and the step between events.
const EPOCH_MS: u64 = 1_700_000_000_000;
const STEP_MS: u64 = 10;

pub fn local(model: &str) -> Target {
    Target::Local {
        worker_id: 7,
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

pub fn usage(input: u64, cached: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: cached,
        output_tokens: output,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

pub fn estimated(mut usage: Usage) -> Usage {
    usage.accounting = Accounting::Estimated;
    usage
}

pub fn candidate(target: Target, expected_cost_usd: f64) -> Candidate {
    Candidate {
        target,
        expected_prefill_tokens: 1_000.0,
        matched_prefix_tokens: 0,
        expected_ttft_ms: 200.0,
        expected_cost_usd,
        quality_prior: 0.6,
        load: None,
    }
}

/// A decision a turn was routed on.
pub fn decision(chosen: Target, considered: Vec<Candidate>) -> DecisionRecord {
    // A local dispatch bills nothing, so it records no card -- exactly as the
    // engine writes it.
    let rate_card = if chosen.is_local() {
        None
    } else {
        Some(HOSTED)
    };
    DecisionRecord {
        chosen,
        rationale: "warmest prefix".into(),
        policy: "affinity".into(),
        isl_tokens: 1_000,
        expected_prefill_tokens: 1_000.0,
        expected_cost_usd: 0.01,
        considered,
        turn_policy_digest: "0123456789abcdef".into(),
        budget_state: BudgetState::Unconstrained,
        rate_card,
        payer: Payer::Deployment,
        billing: Billing::Billed,
        budget_draw: None,
        withheld_providers: Vec::new(),
    }
}

/// A log, built the way the engine writes one.
pub struct Log {
    session_id: SessionId,
    events: Vec<SessionEvent>,
}

impl Log {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: SessionId::new(session_id),
            events: Vec::new(),
        }
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn push(&mut self, kind: SessionEventKind) -> &mut Self {
        let seq = self.events.len() as u64 + 1;
        self.events.push(SessionEvent {
            seq,
            session_id: self.session_id.clone(),
            at_ms: EPOCH_MS + seq * STEP_MS,
            kind,
        });
        self
    }

    pub fn created(&mut self, principal: Option<Principal>) -> &mut Self {
        self.push(SessionEventKind::SessionCreated {
            model_policy: "affinity".into(),
            principal,
            arm: None,
        })
    }

    /// An ordinary turn: one user message in, one answer out.
    pub fn turn(
        &mut self,
        turn_id: &str,
        response_id: &str,
        target: Target,
        u: Usage,
    ) -> &mut Self {
        self.routed_turn(turn_id, response_id, decision(target, Vec::new()), u)
    }

    /// The same, with the decision spelled out — the losing candidates, the
    /// billing mode, whatever the test is about.
    pub fn routed_turn(
        &mut self,
        turn_id: &str,
        response_id: &str,
        decision: DecisionRecord,
        u: Usage,
    ) -> &mut Self {
        let response = ResponseId::new(response_id);
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn_id),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::user_text(format!("ask {turn_id}")),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision,
        });
        self.push(SessionEventKind::OutputTextDelta {
            response_id: response.clone(),
            text: format!("answer {turn_id}"),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::assistant_text(format!("answer {turn_id}"), response.clone()),
        });
        self.push(SessionEventKind::ResponseCompleted {
            response_id: response,
            usage: u,
        })
    }

    /// A turn whose owner died mid-dispatch: started, routed, never terminated.
    pub fn abandoned_turn(&mut self, turn_id: &str, response_id: &str) -> &mut Self {
        let response = ResponseId::new(response_id);
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn_id),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::user_text(format!("ask {turn_id}")),
        });
        self.push(SessionEventKind::Routed {
            response_id: response,
            decision: decision(local("llama"), Vec::new()),
        })
    }

    /// A turn refused before anything was sent: an empty usage, by construction.
    pub fn refused_turn(
        &mut self,
        turn_id: &str,
        response_id: &str,
        reason: IncompleteReason,
    ) -> &mut Self {
        let response = ResponseId::new(response_id);
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn_id),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::user_text(format!("ask {turn_id}")),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(frontier("anthropic", "claude"), Vec::new()),
        });
        self.push(SessionEventKind::ResponseIncomplete {
            response_id: response,
            reason,
            usage: Usage::default(),
        })
    }

    /// A turn that burned a prefill and then failed: billed, and with no answer
    /// and no tool call to put in a payload.
    pub fn truncated_turn(&mut self, turn_id: &str, response_id: &str, u: Usage) -> &mut Self {
        let response = ResponseId::new(response_id);
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn_id),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::user_text(format!("ask {turn_id}")),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(frontier("anthropic", "claude"), Vec::new()),
        });
        self.push(SessionEventKind::ResponseIncomplete {
            response_id: response,
            reason: IncompleteReason::UpstreamError,
            usage: u,
        })
    }

    /// A turn that answered with a tool call rather than with text.
    pub fn tool_call_turn(
        &mut self,
        turn_id: &str,
        response_id: &str,
        call_id: &str,
        name: &str,
        arguments: &str,
    ) -> &mut Self {
        let response = ResponseId::new(response_id);
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn_id),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::user_text(format!("ask {turn_id}")),
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(frontier("anthropic", "claude"), Vec::new()),
        });
        let call = Item {
            response_id: Some(response.clone()),
            ..Item::tool_call(call_id, name, arguments)
        };
        self.push(SessionEventKind::ItemAppended { item: call });
        self.push(SessionEventKind::ResponseCompleted {
            response_id: response,
            usage: usage(1_000, 0, 40),
        })
    }

    /// The next turn, carrying back what the tool returned.
    pub fn tool_result_turn(
        &mut self,
        turn_id: &str,
        response_id: &str,
        call_id: &str,
        output: &str,
    ) -> &mut Self {
        let response = ResponseId::new(response_id);
        self.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new(turn_id),
            response_id: response.clone(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item {
                role: roundhouse_core::item::Role::Tool,
                content: roundhouse_core::item::ItemContent::ToolResult {
                    call_id: call_id.into(),
                    output: output.into(),
                },
                response_id: None,
            },
        });
        self.push(SessionEventKind::Routed {
            response_id: response.clone(),
            decision: decision(frontier("anthropic", "claude"), Vec::new()),
        });
        self.push(SessionEventKind::OutputTextDelta {
            response_id: response.clone(),
            text: "done".into(),
        });
        self.push(SessionEventKind::ItemAppended {
            item: Item::assistant_text("done", response.clone()),
        });
        self.push(SessionEventKind::ResponseCompleted {
            response_id: response,
            usage: usage(2_000, 1_000, 20),
        })
    }
}
