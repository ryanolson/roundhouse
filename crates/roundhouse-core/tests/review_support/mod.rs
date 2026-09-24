// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared fixtures for the frontier review interval suites.
//!
//! Logs are written through the real [`Session`] writer, so every sequence
//! number, response id and configuration replacement is the one the store and
//! the fold produce. Reviews run through the real [`Validator`] against that
//! session's own projection, and their records are committed the way the
//! engine commits them.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use roundhouse_core::control::{Principal, TurnPolicy};
use roundhouse_core::event::{
    IncompleteReason, SessionEvent, SessionEventKind, Usage, ValidationOutcome,
};
use roundhouse_core::ids::{ResponseId, SessionId, SideCallId, TurnId};
use roundhouse_core::interject::{Interjection, InterjectionContext, Interjector};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::routing::{
    CacheLedger, Candidate, DecisionRecord, LocalFeatures, ProviderPricing, SelectionSnapshot,
    Target, TurnSignals,
};
use roundhouse_core::session::{Session, SessionState};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_core::validate::{
    ActionPolicy, Arm, ControlCallDialect, Evidence, IntervalReview, JudgeAnswer, JudgeClient,
    JudgeFailure, Objective, ObjectiveVersion, SideCall, Signal, SignalKind, SteerChannel,
    TriggerConfig, ValidationTerms, Validator, ValidatorConfig,
};

pub const TTL: u64 = 60_000;

pub const ON_TRACK: &str =
    r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#;
pub const OFF_TRACK: &str = r#"{"on_track":false,"confidence":0.8,"divergence":{"at_step":0,"description":"edits unrelated files"},"missing_context":null}"#;
/// Off track with no located divergence: `map` answers `Continue` for it.
pub const OFF_TRACK_UNLOCATED: &str =
    r#"{"on_track":false,"confidence":0.6,"divergence":null,"missing_context":null}"#;
pub const MISSING_CONTEXT: &str = r#"{"on_track":true,"confidence":0.5,"divergence":null,"missing_context":"the test output was not shown"}"#;

/// Routing facts the judge must never see. Each is distinctive enough that a
/// substring match can only mean the value leaked.
pub const ROUTED_PROVIDER: &str = "zephyrcorp";
pub const ROUTED_MODEL: &str = "zeta-ultra-9";
pub const ROUTED_RATIONALE: &str = "cheapest warm pick above the quality floor";
pub const ROUTED_PRICE: f64 = 0.4271;
pub const ROUTED_DIGEST: &str = "4ec325a715649c8e";

pub fn routed_target() -> Target {
    Target::Frontier {
        provider: ROUTED_PROVIDER.into(),
        model: ROUTED_MODEL.into(),
    }
}

/// A developer instruction item, as a client's leading system block
/// canonicalizes: turn configuration rather than history.
pub fn developer(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

pub fn tool_result(call_id: &str, output: &str) -> Item {
    Item {
        role: Role::Tool,
        content: ItemContent::ToolResult {
            call_id: call_id.into(),
            output: output.into(),
        },
        response_id: None,
    }
}

pub fn declared(goal: &str) -> Objective {
    Objective::Declared {
        goal: goal.into(),
        plan_steps: vec![
            format!("{goal}: first step"),
            format!("{goal}: second step"),
        ],
        done_when: format!("{goal}: done"),
    }
}

/// A decision carrying every routing fact the judge must not see, stamped with
/// `objective` the way the engine stamps its selection snapshot.
pub fn decision(turn_index: u64, objective: Option<ObjectiveVersion>) -> DecisionRecord {
    DecisionRecord {
        selection: Some(Box::new(SelectionSnapshot {
            features: LocalFeatures {
                extractor_revision: 1,
                dialect: ControlCallDialect::ClaudeMessages,
                signals: TurnSignals::default(),
                turn_index,
                observed_through_seq: 0,
            },
            selected: routed_target(),
            fallbacks: Vec::new(),
            admitted: None,
            selector: None,
            classifications: None,
            objective,
        })),
        local_quote_skipped: None,
        chosen: routed_target(),
        rationale: ROUTED_RATIONALE.into(),
        policy: "affinity".into(),
        isl_tokens: 1_000,
        expected_prefill_tokens: 1_000.0,
        expected_cost_usd: ROUTED_PRICE,
        considered: vec![Candidate {
            target: routed_target(),
            expected_prefill_tokens: 1_000.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 90.0,
            expected_cost_usd: ROUTED_PRICE,
            quality_prior: 0.9,
            load: None,
        }],
        turn_policy_digest: ROUTED_DIGEST.into(),
        budget_state: Default::default(),
        rate_card: Some(ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        }),
        payer: Default::default(),
        billing: Default::default(),
        budget_draw: None,
        withheld_providers: Vec::new(),
        declared_baseline: None,
        attempts: Vec::new(),
    }
}

pub fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 10,
        ..Usage::default()
    }
}

/// A session log written through the real writer.
pub struct Log {
    pub store: Arc<MemoryStore>,
    pub id: SessionId,
    pub session: Session<MemoryStore>,
}

impl Log {
    /// A session enrolled in `arm`.
    pub async fn enrolled(arm: Option<Arm>) -> Self {
        let store = Arc::new(MemoryStore::new());
        let id = SessionId::new("acme/ada/review");
        store.create_session(&id, "affinity").await.unwrap();
        let mut session = Session::open(
            Arc::clone(&store),
            id.clone(),
            "n1",
            TTL,
            CacheLedger::new(),
        )
        .await
        .unwrap();
        session
            .record_created("affinity", &Principal::new("acme", "ada"), arm)
            .await
            .unwrap();
        Self { store, id, session }
    }

    pub fn state(&self) -> &SessionState {
        self.session.state()
    }

    /// Admit a turn and return its response id.
    pub async fn begin(&mut self, turn: &str, input: Vec<Item>) -> ResponseId {
        self.session
            .begin_turn(TurnId::new(turn), input)
            .await
            .unwrap()
            .response_id()
            .clone()
    }

    /// Record one dispatch of `response_id` and return its `Routed` sequence.
    pub async fn route(
        &mut self,
        response_id: &ResponseId,
        objective: Option<ObjectiveVersion>,
    ) -> u64 {
        let turn_index = self.session.turn_index().saturating_sub(1);
        self.route_with(response_id, decision(turn_index, objective))
            .await
    }

    pub async fn route_with(&mut self, response_id: &ResponseId, decision: DecisionRecord) -> u64 {
        self.session
            .record_routing(response_id, decision)
            .await
            .unwrap();
        self.session.last_seq()
    }

    pub async fn emit(&mut self, response_id: &ResponseId, item: Item) {
        self.session
            .append_emitted(response_id, item)
            .await
            .unwrap();
    }

    pub async fn complete(&mut self, response_id: &ResponseId, text: &str) {
        self.session
            .complete(response_id, Some(text), usage(), None, None)
            .await
            .unwrap();
    }

    pub async fn fail(&mut self, response_id: &ResponseId, partial: &str) {
        self.session
            .mark_incomplete(
                response_id,
                partial,
                IncompleteReason::UpstreamError,
                usage(),
                None,
            )
            .await
            .unwrap();
    }

    /// One ordinary routed text turn; returns its `Routed` sequence.
    pub async fn text_turn(
        &mut self,
        turn: &str,
        input: Vec<Item>,
        answer: &str,
        objective: Option<ObjectiveVersion>,
    ) -> u64 {
        let response = self.begin(turn, input).await;
        let routed = self.route(&response, objective).await;
        self.complete(&response, answer).await;
        routed
    }

    /// Commit what the validator decided, exactly as the engine does.
    pub async fn commit(&mut self, response_id: &ResponseId, interjection: Interjection) {
        match interjection {
            Interjection::Proceed { record } => self.session.record_control(record).await.unwrap(),
            Interjection::Complete {
                item,
                usage,
                record,
            } => self
                .session
                .complete_with_item(response_id, item, usage, record)
                .await
                .unwrap(),
        }
    }

    pub async fn events(&self) -> Vec<SessionEvent> {
        self.store.read_events(&self.id, 0, 100_000).await.unwrap()
    }

    /// A fresh projection of the durable log, as a successor would build it.
    pub async fn replay(&self) -> SessionState {
        SessionState::project(self.store.as_ref(), &self.id, CacheLedger::new(), None)
            .await
            .unwrap()
    }
}

/// A judge that answers from a script and records what it was shown.
pub struct ScriptedJudge {
    answers: Mutex<Vec<Result<JudgeAnswer, JudgeFailure>>>,
    asked: AtomicUsize,
    seen: Mutex<Vec<(String, String)>>,
}

impl ScriptedJudge {
    pub fn new(answers: Vec<Result<JudgeAnswer, JudgeFailure>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().rev().collect()),
            asked: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }

    pub fn answering(raws: &[&str]) -> Arc<Self> {
        Self::new(raws.iter().map(|raw| Ok(answer(raw))).collect())
    }

    pub fn asked(&self) -> usize {
        self.asked.load(Ordering::Acquire)
    }

    /// The `(system prompt, brief)` of the `n`th consult.
    pub fn saw(&self, n: usize) -> (String, String) {
        self.seen.lock().unwrap()[n].clone()
    }
}

pub fn answer(raw: &str) -> JudgeAnswer {
    JudgeAnswer {
        raw: raw.to_string(),
        usage: Usage {
            input_tokens: 4_000,
            output_tokens: 40,
            ..Usage::default()
        },
        target: Target::Frontier {
            provider: "judgeco".into(),
            model: "reviewer".into(),
        },
    }
}

#[async_trait]
impl JudgeClient for ScriptedJudge {
    async fn consult(
        &self,
        _side_call: &SideCall<'_>,
        system_prompt: &str,
        brief: &str,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        self.asked.fetch_add(1, Ordering::AcqRel);
        self.seen
            .lock()
            .unwrap()
            .push((system_prompt.to_string(), brief.to_string()));
        self.answers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Err(JudgeFailure::Unavailable))
    }
}

/// Fires on every turn the gate admits, so a test is about what follows.
pub struct AlwaysFires;

impl Signal for AlwaysFires {
    fn kind(&self) -> SignalKind {
        SignalKind::NoProgressRepeat
    }

    fn detect(&self, _evidence: &Evidence<'_>) -> Option<String> {
        Some("this fixture's signal fires on every turn the gate admits".into())
    }
}

/// A validator whose gate is open from the second turn onward.
pub fn validator_over(judge: Arc<ScriptedJudge>, section_bytes: usize) -> Validator {
    Validator::new(
        judge,
        ValidatorConfig {
            trigger: TriggerConfig {
                tokens_between_validations: 0,
                cooldown_ms: 0,
                max_consecutive_interventions: 1_000,
                max_validations_per_session: 1_000,
            },
            interval_section_bytes: section_bytes,
            arm_salt: "review-fixture".into(),
            ..ValidatorConfig::default()
        },
    )
    .with_signals(vec![Box::new(AlwaysFires)])
}

/// Membership terms: observe only, the shipped channel.
pub fn observing() -> ValidationTerms {
    ValidationTerms {
        action: ActionPolicy {
            channel: SteerChannel::Off,
            ..ActionPolicy::default()
        },
        ..ValidationTerms::default()
    }
}

/// Ask `validator` about the turn `response_id` has just opened, for a Claude
/// Code session.
pub async fn consider(
    validator: &Validator,
    terms: &ValidationTerms,
    state: &SessionState,
    response_id: &ResponseId,
    objective: Objective,
) -> Interjection {
    consider_on(
        validator,
        terms,
        state,
        response_id,
        objective,
        ControlCallDialect::ClaudeMessages,
    )
    .await
}

/// The same, for a session whose client spells control calls as `dialect`.
pub async fn consider_on(
    validator: &Validator,
    terms: &ValidationTerms,
    state: &SessionState,
    response_id: &ResponseId,
    objective: Objective,
    dialect: ControlCallDialect,
) -> Interjection {
    let session_id = SessionId::new("acme/ada/review");
    let principal = Principal::new("acme", "ada");
    let side_call_id = SideCallId::generate();
    let policy = TurnPolicy::unrestricted();
    validator
        .consider(&InterjectionContext {
            state,
            response_id,
            turn_policy: &policy,
            objective,
            side_call: SideCall {
                session_id: &session_id,
                id: &side_call_id,
                principal: &principal,
                budget: None,
            },
            validation: Some(terms),
            dialect,
        })
        .await
}

/// Admit `turn`, review it, and commit the review. Returns the response id of
/// the reviewed turn, which is still open, and what the validator decided.
pub async fn review_turn(
    log: &mut Log,
    validator: &Validator,
    terms: &ValidationTerms,
    turn: &str,
    input: Vec<Item>,
    objective: Objective,
) -> (ResponseId, Interjection) {
    let response = log.begin(turn, input).await;
    let decided = consider(validator, terms, log.state(), &response, objective).await;
    log.commit(&response, decided.clone()).await;
    (response, decided)
}

/// The interval review a validator attached, if any.
pub fn interval_of(interjection: &Interjection) -> Option<IntervalReview> {
    let record = match interjection {
        Interjection::Proceed { record } | Interjection::Complete { record, .. } => record,
    };
    record.kinds().iter().find_map(|kind| match kind {
        SessionEventKind::ValidationDecided {
            outcome: ValidationOutcome::Judged { interval, .. },
            ..
        } => interval.as_deref().cloned(),
        _ => None,
    })
}

/// The lines a brief writes itself, as opposed to quoted transcript.
pub fn scaffolding(brief: &str) -> Vec<&str> {
    brief
        .lines()
        .filter(|line| !line.trim_start().starts_with('>'))
        .collect()
}

/// The reviewed-turns section of a brief, if it has one.
pub fn section(brief: &str) -> Option<&str> {
    brief
        .find(roundhouse_core::validate::INTERVAL_SECTION_HEADING)
        .map(|at| &brief[at..])
}
