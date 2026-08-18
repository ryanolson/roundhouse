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

use crate::control::PrincipalKey;
use crate::event::{SessionEvent, SessionObserver};
use crate::routing::Target;

pub use fold::{MetricsFold, Scope};
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
        MetricsSnapshot::build(&fold, Scope::Deployment, config, generated_at_ms)
    }

    /// The same report, restricted to one principal's share of the same fold.
    ///
    /// What a turn key is answered with. A separate method rather than a
    /// [`Scope`] argument on [`Self::snapshot`] so that "this document is
    /// somebody's only" is a decision named at the call site, in the surface
    /// that serves it. The two are one function underneath — the scope seam is
    /// [`MetricsSnapshot::build`] — so there is no second pricing walk for
    /// these two entry points to disagree over.
    pub fn snapshot_for(
        &self,
        scope: &PrincipalKey,
        config: &MetricsConfig,
        generated_at_ms: u64,
    ) -> MetricsSnapshot {
        let fold = self.fold.read().unwrap_or_else(|e| e.into_inner());
        MetricsSnapshot::build(&fold, Scope::Principal(scope), config, generated_at_ms)
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
    // The log fixtures live with the fold they build logs for; see
    // `fold::tests`. One builder means one clock, so a test that compares a
    // window across two logs is asserting about the fold rather than about two
    // fixtures that happened to agree.
    use crate::control::PrincipalKey;
    use crate::event::{Accounting, IncompleteReason, SessionEventKind, Usage};
    use crate::ids::{ResponseId, TurnId};
    use crate::metrics::fold::tests::{LogBuilder, candidate, frontier, local, principal, usage};
    use crate::routing::{DecisionRecord, ProviderPricing};

    const HOSTED: ProviderPricing = ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    };

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
        MetricsSnapshot::build(fold, Scope::Deployment, &config(), 9_999)
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
                turn_policy_digest: String::new(),
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
                turn_policy_digest: String::new(),
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

    /// A scoped report must be scoped in *every* field, not only in its rows.
    ///
    /// The failure this pins is the quiet one: a document whose money is
    /// filtered to one principal but whose session count, turn count and event
    /// window are still deployment-wide reads as correct, and discloses the
    /// size and activity window of every other tenant to anyone holding a turn
    /// key. Filtering the rows is the easy half.
    #[test]
    fn a_scoped_snapshot_is_scoped_in_every_field_not_only_its_rows() {
        let acme = principal("acme", "ada");
        let globex = principal("globex", "bob");

        let mut mine = LogBuilder::new("acme/ada/main");
        mine.created(Some(acme.clone())).turn(
            "r1",
            frontier("anthropic", "claude"),
            vec![],
            usage(1_000, 0, 100, 0),
        );
        let mut theirs = LogBuilder::new("globex/bob/main");
        theirs
            .created(Some(globex.clone()))
            .turn(
                "r2",
                frontier("anthropic", "claude"),
                vec![],
                usage(2_000, 0, 200, 0),
            )
            .turn(
                "r3",
                frontier("anthropic", "claude"),
                vec![],
                usage(4_000, 0, 400, 0),
            );
        // A log from before the control plane: it must not be silently added to
        // anyone's row, and it must not vanish from the deployment's.
        let mut legacy = LogBuilder::new("legacy");
        legacy.turn(
            "r4",
            frontier("anthropic", "claude"),
            vec![],
            usage(8_000, 0, 800, 0),
        );

        let mut fold = MetricsFold::new();
        fold.extend(mine.events());
        fold.extend(theirs.events());
        fold.extend(legacy.events());

        let config = config();
        let deployment = MetricsSnapshot::build(&fold, Scope::Deployment, &config, 9_999);
        let scoped = MetricsSnapshot::build(
            &fold,
            Scope::Principal(&PrincipalKey::from(&acme)),
            &config,
            9_999,
        );

        assert_eq!(deployment.sessions, 3);
        assert_eq!(deployment.turns, 4);
        assert_eq!(scoped.sessions, 1, "one principal, one session");
        assert_eq!(scoped.turns, 1);
        assert_eq!(scoped.calls, 1);
        assert_eq!(scoped.tokens.input, 1_000);

        // The window is the caller's own traffic, not the deployment's. Read
        // off the fixture rather than restated, so a change to the builder's
        // clock cannot make this pass by coincidence.
        let mine_first = mine.events().first().expect("the log is non-empty").at_ms;
        let mine_last = mine.events().last().expect("the log is non-empty").at_ms;
        assert_eq!(scoped.first_event_at_ms, Some(mine_first));
        assert_eq!(scoped.last_event_at_ms, Some(mine_last));
        assert!(
            deployment.last_event_at_ms > scoped.last_event_at_ms,
            "the fixture must actually distinguish the two windows"
        );

        // The scoped documents adding up to the deployment's was asserted here
        // and no longer is, deliberately. It was a real claim while two folds
        // were accumulated side by side; now the deployment's rows, turns and
        // sessions are *summed out of* the per-principal ones on the way out
        // (see `MetricsFold::view`), so the assertion reduces to `x == x`. The
        // property is now held by construction, and a test that cannot fail is
        // worse than no test: it reads as coverage.
        //
        // The half that is still a claim is above — a scoped document that
        // filtered its rows but not its window, session count or turn count.
        // That one can regress, so that one stays.
    }
}
