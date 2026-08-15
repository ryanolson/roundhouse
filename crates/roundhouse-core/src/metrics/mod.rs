// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Metrics as a projection of the event log.
//!
//! Nothing here is separately recorded. Token counts, dollars, and the savings
//! figure are all folded out of the same append-only log that already carries
//! the conversation and the routing ledger, for the reason stated in
//! [`crate::store`]: one write path means the dashboard cannot disagree with
//! the audit trail. A counter incremented alongside the log would drift the
//! first time a turn failed between the two writes, and the drift would be
//! silent and permanent.
//!
//! The fold is [`MetricsFold::apply`], a pure function of the events it is
//! given, so the same code serves two jobs. A live process feeds it each event
//! as it is appended and answers `/v1/metrics` from memory; a process that
//! wants to rebuild — after a restart, or to check the live numbers — replays
//! the log through the identical fold and must get the identical answer. That
//! equivalence is what [`MetricsFold`] is tested on.
//!
//! ## The two axes
//!
//! A turn is grouped twice, because the two questions are different. **Who
//! serves it** — Anthropic, OpenAI, our own fleet — is [`ModelKey::provider`],
//! and it is what a rate card attaches to. **Where it runs** — on hardware we
//! own or on someone's endpoint — is [`ServingMode`], and it is what the
//! savings argument turns on.
//!
//! ## Where the money comes from
//!
//! Three figures, and they are not equally solid. Keeping them apart is the
//! point of [`Savings`]:
//!
//! - [`Savings::frontier_spend_usd`] is money that left the building. Measured
//!   token counts, published rate card.
//! - [`Savings::cache_savings_usd`] is a discount a provider actually applied,
//!   reconstructed from the cache-read tokens it reported and the gap between
//!   its two published rates. Measured.
//! - [`Savings::routing_savings_usd`] is a **counterfactual**: what our own
//!   fleet's traffic would have cost had it gone to a comparable hosted model
//!   instead. There is no measurement of a call that never happened, so this
//!   rests entirely on the correlary chosen in [`pricing`], and it is only as
//!   good as that choice.
//!
//! A single total would hide that distinction, so the snapshot reports all
//! three and lets the reader decide which claim to make.

//! ## Layout
//!
//! Three modules, split along the two seams the design already had. [`fold`]
//! turns events into token counters and touches no money. [`snapshot`] applies
//! a rate card to those counters and owns every dollar figure and every wire
//! type. [`pricing`] owns the correlary machinery [`snapshot`] consults. This
//! module keeps only the vocabulary all three share and the live recorder that
//! drives them, and re-exports the rest so callers see one surface.

pub mod fold;
pub mod pricing;
pub mod snapshot;

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::event::{SessionEvent, SessionObserver};
use crate::routing::Target;

pub use fold::MetricsFold;
pub use pricing::{
    Correlary, DEFAULT_CAPABILITY_BAND, IncoherentCorrelary, PricedBasis, ReferenceModel,
    ShadowPricing, TokenShape,
};
pub use snapshot::{
    Coverage, MetricsConfig, MetricsSnapshot, ModelAccounting, ModelMetrics, ProviderMetrics,
    Rollup, Savings, ServingModeMetrics, TokenBreakdown,
};

/// The provider name local targets are grouped under.
///
/// [`Target::Local`] carries a worker and a model but no provider, because
/// locally there is nobody to bill. The rollup still needs a name in that
/// column, and the fleet's own is the honest one — the alternative, an empty
/// string or "none", reads as missing data rather than as the deliberate
/// absence of a vendor.
pub const LOCAL_PROVIDER: &str = "dynamo";

/// Whether a target runs on hardware we own.
///
/// The axis the whole savings argument is stated on, which is why it is its own
/// type rather than a `bool` named `is_local`: the two sides are accounted for
/// completely differently, and a boolean at a call site does not say which way
/// round it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingMode {
    /// Served by our own Dynamo fleet. Bills nothing; costs GPU time.
    Local,
    /// Issued to an external endpoint. Bills real money.
    Frontier,
}

impl ServingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServingMode::Local => "local",
            ServingMode::Frontier => "frontier",
        }
    }
}

/// One row of the breakdown.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelKey {
    pub mode: ServingMode,
    pub provider: String,
    pub model: String,
}

impl ModelKey {
    pub fn from_target(target: &Target) -> Self {
        match target {
            // Deliberately not keyed by worker: which of our GPUs served a turn
            // is a fleet-balance question, and putting it here would split one
            // model's row into one row per worker and make every per-model
            // number meaningless at a glance.
            Target::Local { model, .. } => Self {
                mode: ServingMode::Local,
                provider: LOCAL_PROVIDER.to_string(),
                model: model.clone(),
            },
            Target::Frontier { provider, model } => Self {
                mode: ServingMode::Frontier,
                provider: provider.clone(),
                model: model.clone(),
            },
        }
    }
}

/// Process-wide metrics, maintained as sessions run.
///
/// Cheap to clone and shared by every handler. The lock is `std` rather than
/// `tokio` on purpose: nothing inside the critical section awaits, and an async
/// lock would suggest it might.
#[derive(Clone, Default)]
pub struct MetricsRecorder {
    fold: Arc<RwLock<MetricsFold>>,
}

impl MetricsRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold events in. Safe to call with events already seen — see
    /// [`MetricsFold`] on idempotency, which is what lets a session's replay
    /// on open feed this without double counting what a live feed already had.
    pub fn record(&self, events: &[SessionEvent]) {
        // A poisoned lock is recovered rather than propagated. The fold holds
        // counters, not invariants another thread's panic could have corrupted
        // halfway, and taking the whole metrics surface down for the life of
        // the process because one request panicked is the worse failure.
        let mut fold = self.fold.write().unwrap_or_else(|e| e.into_inner());
        fold.extend(events);
    }

    pub fn snapshot(&self, config: &MetricsConfig, generated_at_ms: u64) -> MetricsSnapshot {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        MetricsSnapshot::build(&fold, config, generated_at_ms)
    }
}

impl SessionObserver for MetricsRecorder {
    fn observe(&self, events: &[SessionEvent]) {
        self.record(events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Accounting, IncompleteReason, SessionEvent, SessionEventKind, Usage};
    use crate::ids::{ResponseId, SessionId, TurnId};
    use crate::routing::{Candidate, DecisionRecord, ProviderPricing};

    const HOSTED: ProviderPricing = ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    };

    fn local(model: &str) -> Target {
        Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: model.into(),
        }
    }

    fn frontier(provider: &str, model: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn candidate(target: Target, cost: f64) -> Candidate {
        Candidate {
            target,
            expected_prefill_tokens: 0.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 0.0,
            expected_cost_usd: cost,
            quality_prior: 0.6,
            load: None,
        }
    }

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_tokens: reasoning,
            accounting: Accounting::Reported,
        }
    }

    /// A minimal session log: one turn routed to `target` and completed.
    ///
    /// Built by hand rather than by driving the engine so the fold can be
    /// tested against logs the engine cannot currently produce — a provider
    /// that reported nothing, a dispatch that died before sending.
    struct LogBuilder {
        session: SessionId,
        events: Vec<SessionEvent>,
        at_ms: u64,
    }

    impl LogBuilder {
        fn new(session: &str) -> Self {
            Self {
                session: SessionId::new(session),
                events: Vec::new(),
                at_ms: 1_000,
            }
        }

        fn push(&mut self, kind: SessionEventKind) -> &mut Self {
            self.at_ms += 10;
            self.events.push(SessionEvent {
                seq: self.events.len() as u64 + 1,
                session_id: self.session.clone(),
                at_ms: self.at_ms,
                kind,
            });
            self
        }

        fn turn(
            &mut self,
            response: &str,
            target: Target,
            considered: Vec<Candidate>,
            usage: Usage,
        ) -> &mut Self {
            let response_id = ResponseId::new(response);
            self.push(SessionEventKind::TurnStarted {
                turn_id: TurnId::new(format!("turn-{response}")),
                response_id: response_id.clone(),
            });
            self.push(SessionEventKind::Routed {
                response_id: response_id.clone(),
                decision: DecisionRecord {
                    chosen: target,
                    rationale: "test".into(),
                    policy: "test".into(),
                    isl_tokens: usage.input_tokens,
                    expected_prefill_tokens: 0.0,
                    expected_cost_usd: 0.0,
                    considered,
                },
            });
            self.push(SessionEventKind::ResponseCompleted { response_id, usage });
            self
        }

        fn events(&self) -> &[SessionEvent] {
            &self.events
        }
    }

    fn config() -> MetricsConfig {
        MetricsConfig::new(
            ShadowPricing::new(vec![ReferenceModel {
                provider: "anthropic".into(),
                model: "claude".into(),
                pricing: HOSTED,
                quality_prior: 0.6,
            }])
            .declare("llama", "anthropic", "claude", "matched on our eval suite"),
        )
        .with_default_local_quality(0.6)
    }

    fn snapshot(fold: &MetricsFold) -> MetricsSnapshot {
        MetricsSnapshot::build(fold, &config(), 9_999)
    }

    #[test]
    fn a_replayed_log_folds_exactly_once() {
        let mut log = LogBuilder::new("s1");
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(10_000, 8_000, 500, 0),
        );

        let mut fold = MetricsFold::new();
        assert_eq!(fold.extend(log.events()), log.events().len());
        let once = snapshot(&fold);

        // The same events again — a live feed and a rebuild overlapping, which
        // is the normal case after a restart.
        assert_eq!(fold.extend(log.events()), 0, "no event should be new twice");
        let twice = snapshot(&fold);

        assert_eq!(once.calls, twice.calls);
        assert_eq!(once.tokens, twice.tokens);
        assert_eq!(
            once.savings.frontier_spend_usd,
            twice.savings.frontier_spend_usd
        );
    }

    #[test]
    fn a_rebuild_from_the_log_matches_a_live_feed() {
        let mut log = LogBuilder::new("s1");
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(10_000, 8_000, 500, 120),
        );
        log.turn(
            "r2",
            local("llama"),
            vec![candidate(frontier("anthropic", "claude"), 0.042)],
            usage(12_000, 9_000, 600, 0),
        );

        // Live: one event at a time, as the engine appends them.
        let mut live = MetricsFold::new();
        for event in log.events() {
            live.apply(event);
        }
        // Rebuild: the whole log in one sweep, as a restarted process reads it.
        let mut rebuilt = MetricsFold::new();
        rebuilt.extend(log.events());

        let live = snapshot(&live);
        let rebuilt = snapshot(&rebuilt);
        assert_eq!(live.tokens, rebuilt.tokens);
        assert_eq!(live.calls, rebuilt.calls);
        assert_eq!(live.savings.total_usd, rebuilt.savings.total_usd);
        assert_eq!(live.models.len(), rebuilt.models.len());
    }

    #[test]
    fn local_and_frontier_are_grouped_on_both_axes() {
        let mut log = LogBuilder::new("s1");
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(10_000, 0, 500, 0),
        );
        log.turn("r2", local("llama"), vec![], usage(20_000, 15_000, 800, 0));

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let snapshot = snapshot(&fold);

        assert_eq!(snapshot.models.len(), 2);
        assert_eq!(snapshot.serving_modes.len(), 2);

        let local_mode = snapshot
            .serving_modes
            .iter()
            .find(|m| m.mode == ServingMode::Local)
            .unwrap();
        assert_eq!(local_mode.totals.tokens.input, 20_000);
        assert_eq!(local_mode.totals.billed_usd, 0.0, "local bills nothing");
        assert!(local_mode.totals.shadow_usd > 0.0, "local is shadow-priced");

        // Local traffic is grouped under the fleet, not under a vendor.
        let local_row = snapshot
            .models
            .iter()
            .find(|m| m.mode() == ServingMode::Local)
            .unwrap();
        assert_eq!(local_row.provider, LOCAL_PROVIDER);
    }

    #[test]
    fn savings_separate_measured_discounts_from_counterfactual_routing() {
        let mut log = LogBuilder::new("s1");
        // A hosted call with a warm cache: a real, measured discount.
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(100_000, 90_000, 1_000, 0),
        );
        // A local call: no money spent, so the saving is a counterfactual.
        log.turn(
            "r2",
            local("llama"),
            vec![candidate(frontier("anthropic", "claude"), 0.05)],
            usage(100_000, 90_000, 1_000, 0),
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let snapshot = snapshot(&fold);

        // 90k cached tokens at (3.75 write - 0.30 cached) per Mtok.
        let expected_cache = 90_000.0 * (3.75 - 0.3) * 1e-6;
        assert!((snapshot.savings.cache_savings_usd - expected_cache).abs() < 1e-12);

        // The local turn priced on its correlary, carrying its cache ratio.
        let expected_shadow = 10_000.0 * 3.75e-6 + 90_000.0 * 0.3e-6 + 1_000.0 * 15.0e-6;
        assert!((snapshot.savings.routing_savings_usd - expected_shadow).abs() < 1e-12);

        assert!(
            (snapshot.savings.total_usd
                - (snapshot.savings.cache_savings_usd + snapshot.savings.routing_savings_usd))
                .abs()
                < 1e-12
        );
        // The router's own quote for the road not taken, kept apart from the total.
        assert!((snapshot.savings.routing_savings_at_decision_usd - 0.05).abs() < 1e-12);
        assert!(snapshot.savings.frontier_spend_usd > 0.0);
    }

    #[test]
    fn an_unreported_call_is_marked_rather_than_counted_as_free() {
        let mut log = LogBuilder::new("s1");
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            Usage {
                accounting: Accounting::Estimated,
                ..usage(10_000, 0, 400, 0)
            },
        );
        log.turn(
            "r2",
            frontier("anthropic", "claude"),
            vec![],
            usage(10_000, 0, 400, 0),
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let snapshot = snapshot(&fold);

        assert_eq!(snapshot.coverage.calls, 2);
        assert_eq!(snapshot.coverage.estimated_calls, 1);
        assert!((snapshot.coverage_fraction - 0.5).abs() < 1e-12);
    }

    #[test]
    fn a_dispatch_that_never_reached_the_provider_is_not_a_call() {
        let mut log = LogBuilder::new("s1");
        let response_id = ResponseId::new("r1");
        log.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new("t1"),
            response_id: response_id.clone(),
        });
        log.push(SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: DecisionRecord {
                chosen: frontier("anthropic", "claude"),
                rationale: "test".into(),
                policy: "test".into(),
                isl_tokens: 10_000,
                expected_prefill_tokens: 10_000.0,
                expected_cost_usd: 0.03,
                considered: vec![],
            },
        });
        // Failed before anything was sent: empty usage is the engine's way of
        // saying the prompt never reached the provider.
        log.push(SessionEventKind::ResponseIncomplete {
            response_id,
            reason: IncompleteReason::UpstreamError,
            usage: Usage::default(),
        });

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let snapshot = snapshot(&fold);

        assert_eq!(
            snapshot.calls, 0,
            "a turn that never dispatched is not a call"
        );
        assert_eq!(snapshot.turns, 1, "but it was still a turn");
        assert_eq!(snapshot.tokens.total, 0);
    }

    #[test]
    fn an_incomplete_that_burned_tokens_is_still_billed() {
        let mut log = LogBuilder::new("s1");
        let response_id = ResponseId::new("r1");
        log.push(SessionEventKind::TurnStarted {
            turn_id: TurnId::new("t1"),
            response_id: response_id.clone(),
        });
        log.push(SessionEventKind::Routed {
            response_id: response_id.clone(),
            decision: DecisionRecord {
                chosen: frontier("anthropic", "claude"),
                rationale: "test".into(),
                policy: "test".into(),
                isl_tokens: 10_000,
                expected_prefill_tokens: 10_000.0,
                expected_cost_usd: 0.03,
                considered: vec![],
            },
        });
        log.push(SessionEventKind::ResponseIncomplete {
            response_id,
            reason: IncompleteReason::UpstreamError,
            usage: usage(10_000, 0, 0, 0),
        });

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let snapshot = snapshot(&fold);

        assert_eq!(snapshot.calls, 1);
        assert!(
            snapshot.savings.frontier_spend_usd > 0.0,
            "a prefill we were billed for is spend even though the answer never came"
        );
    }

    #[test]
    fn sessions_are_counted_across_separate_logs() {
        let mut a = LogBuilder::new("s1");
        a.turn("r1", local("llama"), vec![], usage(1_000, 0, 100, 0));
        let mut b = LogBuilder::new("s2");
        b.turn("r2", local("llama"), vec![], usage(1_000, 0, 100, 0));

        let mut fold = MetricsFold::new();
        fold.extend(a.events());
        fold.extend(b.events());
        let snapshot = snapshot(&fold);

        assert_eq!(snapshot.sessions, 2);
        assert_eq!(snapshot.calls, 2);
        assert_eq!(
            snapshot.models.len(),
            1,
            "the same model across two sessions is one row"
        );
    }

    #[test]
    fn reasoning_tokens_stay_inside_output() {
        let mut log = LogBuilder::new("s1");
        log.turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(1_000, 0, 900, 700),
        );

        let mut fold = MetricsFold::new();
        fold.extend(log.events());
        let snapshot = snapshot(&fold);

        assert_eq!(snapshot.tokens.output, 900);
        assert_eq!(snapshot.tokens.reasoning, 700);
        assert_eq!(
            snapshot.tokens.total, 1_900,
            "reasoning is part of output, not an addition to it"
        );
    }
}
