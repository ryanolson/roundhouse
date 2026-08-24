// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The doubles that stand in for the two things a validate/steer fixture must
//! not be about: what a hosted judge would say, and when a signal should fire.
//!
//! Promoted out of `tests/validate_loop.rs` when a second suite — M9's
//! real-binary end-to-end — needed the *same* loop turned on the same way.
//! Copying them would have made the trigger's turn arithmetic two independent
//! restatements of one rule, and the M9 suite drives three `codex` processes
//! to reach a steer: a fixture that silently disagreed with M6's about when a
//! validation runs would fail three minutes later with nothing pointing here.
//!
//! What is deliberately *not* here: the rig, the enrollment and the engine
//! wiring. Those differ between a suite driving `run_turn` directly and one
//! driving a socket, and a shared constructor covering both would take
//! arguments that mean nothing to either.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use roundhouse_core::event::{Accounting, Usage};
use roundhouse_core::routing::Target;
use roundhouse_core::validate::{
    Evidence, JudgeAnswer, JudgeClient, JudgeFailure, SideCall, Signal, SignalKind, TriggerConfig,
};
use roundhouse_fleet::FrontierModelSpec;

use super::MINUTE;

/// A verdict that maps to `Continue`: the cheap default, and the one that lets
/// a test assert "the turn ran exactly as it would have".
pub const ON_TRACK: &str = r#"{"on_track":true,"confidence":0.9,"divergence":null,
    "missing_context":null}"#;

/// A verdict with a located divergence, which is what any action stronger than
/// `Continue` requires.
pub const OFF_TRACK: &str = r#"{"on_track":false,"confidence":0.8,
    "divergence":{"at_step":2,"description":"editing a file the task did not name"},
    "missing_context":null}"#;

// ---------------------------------------------------------------------------
// The judge
// ---------------------------------------------------------------------------

/// The model the judge runs on.
///
/// Deliberately **not** in the engine's catalog. That is what makes "the side
/// call books under its own model row" an assertion about the fold rather than
/// about which of two hosted models the router happened to pick, and what makes
/// "it never reaches the cache ledger" checkable at all: a target the ledger
/// has never been told about is warm only if something warmed it.
pub fn judge_spec() -> FrontierModelSpec {
    FrontierModelSpec {
        provider: "judgeco".into(),
        model: "reviewer-1".into(),
        wire_protocol: roundhouse_fleet::WireProtocol::AnthropicMessages,
        cache_model: roundhouse_core::routing::CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: roundhouse_core::routing::ProviderPricing {
            input_per_mtok_usd: 1.0,
            cached_input_per_mtok_usd: 0.1,
            cache_write_per_mtok_usd: 1.25,
            output_per_mtok_usd: 5.0,
        },
        quality_prior: 0.9,
        base_ttft_ms: 100.0,
        ttft_ms_per_uncached_token: 0.001,
    }
}

pub fn judge_target() -> Target {
    judge_spec().target()
}

/// What the judge's side call is reported to have cost.
///
/// Non-zero on every axis and non-round, so an assertion that a model row
/// carries *this* usage cannot be satisfied by a default.
pub fn judge_usage() -> Usage {
    Usage {
        input_tokens: 1_100,
        cached_input_tokens: 300,
        output_tokens: 47,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

/// A judge that answers from a script and records everything it was asked.
pub struct ScriptedJudge {
    answers: Mutex<Vec<Result<JudgeAnswer, JudgeFailure>>>,
    asked: AtomicUsize,
    /// The cache key each consult was made under — the one isolation a caller
    /// of this trait can get wrong without anything else noticing.
    keys: Mutex<Vec<String>>,
    /// Every brief the judge was shown, so a test can assert on what it saw
    /// rather than only on what it said.
    briefs: Mutex<Vec<String>>,
    /// Awaited before each consult answers, when a test holds it shut.
    release: Option<Arc<tokio::sync::Notify>>,
}

impl ScriptedJudge {
    pub fn new(answers: Vec<Result<JudgeAnswer, JudgeFailure>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().rev().collect()),
            asked: AtomicUsize::new(0),
            keys: Mutex::new(Vec::new()),
            briefs: Mutex::new(Vec::new()),
            release: None,
        })
    }

    /// A judge that answers `raw` to every consult, forever.
    pub fn always(raw: &str) -> Arc<Self> {
        Self::new(vec![Ok(answer(raw)), Ok(answer(raw)), Ok(answer(raw))])
    }

    /// A judge whose verdicts change from consult to consult.
    ///
    /// The fixture a test needs when the *sequence* is its subject rather than
    /// the verdict — a session that goes off track once and recovers, which is
    /// the only shape in which a turn served under a still-active escalation is
    /// reachable at all. Under `always(OFF_TRACK)` the intervention ladder
    /// claims every turn after the first (escalate, then steer, then halt), so
    /// nothing dispatches for a later assertion to read.
    pub fn answering(raws: &[&str]) -> Arc<Self> {
        Self::new(raws.iter().map(|raw| Ok(answer(raw))).collect())
    }

    /// A judge that will not answer until `release` is notified.
    pub fn blocking(raw: &str, release: Arc<tokio::sync::Notify>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(vec![Ok(answer(raw))]),
            asked: AtomicUsize::new(0),
            keys: Mutex::new(Vec::new()),
            briefs: Mutex::new(Vec::new()),
            release: Some(release),
        })
    }

    pub fn asked(&self) -> usize {
        self.asked.load(Ordering::Acquire)
    }

    pub fn keys(&self) -> Vec<String> {
        self.keys.lock().expect("recording").clone()
    }

    pub fn briefs(&self) -> Vec<String> {
        self.briefs.lock().expect("recording").clone()
    }
}

fn answer(raw: &str) -> JudgeAnswer {
    JudgeAnswer {
        raw: raw.to_string(),
        usage: judge_usage(),
        target: judge_target(),
    }
}

#[async_trait]
impl JudgeClient for ScriptedJudge {
    async fn consult(
        &self,
        side_call: &SideCall<'_>,
        _system_prompt: &str,
        brief: &str,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        self.asked.fetch_add(1, Ordering::AcqRel);
        self.keys
            .lock()
            .expect("recording")
            .push(format!("{}#validate", side_call.session_id));
        self.briefs
            .lock()
            .expect("recording")
            .push(brief.to_string());
        if let Some(release) = &self.release {
            release.notified().await;
        }
        self.answers
            .lock()
            .expect("script")
            .pop()
            .unwrap_or(Err(JudgeFailure::Unavailable))
    }
}

// ---------------------------------------------------------------------------
// The signal
// ---------------------------------------------------------------------------

/// A signal that always fires.
///
/// The conjunction the trigger gates on is a *gate* and a *signal*, and this
/// file's subject is everything downstream of the trigger. `trigger.rs` owns
/// the question of when a signal should fire, with its own probes and controls;
/// arranging a real ping-pong here would make every assertion below partly
/// about signal detection.
pub struct AlwaysFires;

impl Signal for AlwaysFires {
    fn kind(&self) -> SignalKind {
        SignalKind::NoProgressRepeat
    }

    fn detect(&self, _evidence: &Evidence<'_>) -> Option<String> {
        Some("this fixture's signal fires on every turn the gate admits".into())
    }
}

// ---------------------------------------------------------------------------
// The deployment under test
// ---------------------------------------------------------------------------

/// A trigger whose gate is open from the second turn onwards.
///
/// Every budget here is set out of the way on purpose: what the gate does with
/// each of them is `trigger.rs`'s subject, tested there arm by arm. Turn 0 is
/// still excluded, because that rule is not configurable and is what makes the
/// first turn of every fixture below an honest unvalidated control.
pub fn open_trigger() -> TriggerConfig {
    TriggerConfig {
        tokens_between_validations: 0,
        cooldown_ms: 0,
        max_consecutive_interventions: 8,
        max_validations_per_session: 8,
    }
}
