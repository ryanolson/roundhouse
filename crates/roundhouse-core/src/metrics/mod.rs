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

pub mod pricing;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::event::{Accounting, SessionEvent, SessionEventKind, Usage};
use crate::ids::{ResponseId, SessionId, TurnId};
use crate::routing::{DecisionRecord, Target};

pub use pricing::{
    Correlary, CorrelaryBasis, DEFAULT_CAPABILITY_BAND, IncoherentCorrelary, PricedBasis,
    ReferenceModel, ShadowPricing, TokenShape,
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

// ---------------------------------------------------------------------------
// The fold
// ---------------------------------------------------------------------------

/// Raw counters for one [`ModelKey`], before any rate card is applied.
///
/// Deliberately money-free. Prices are configuration and they change; folding
/// dollars in here would freeze whatever rate card happened to be loaded when a
/// turn ran, and a corrected price would then require replaying every session.
/// Tokens are facts, so tokens are what is accumulated.
#[derive(Debug, Clone, Default, PartialEq)]
struct Counters {
    calls: u64,
    /// Calls whose usage the provider never reported. See [`Accounting`].
    estimated_calls: u64,
    /// Tokens the provider itself counted.
    reported_usage: Usage,
    /// Tokens Roundhouse counted because the provider did not.
    ///
    /// Kept apart from `reported_usage` rather than summed into it, and this is
    /// the whole point: pricing is linear in tokens, so two accumulators can be
    /// priced independently at no cost, while one accumulator makes the split
    /// unrecoverable the instant the first estimated call lands. Merging first
    /// and reporting a call-weighted coverage ratio afterwards does not
    /// substitute — a 50%-of-calls ratio is consistent with 95% or 5% of the
    /// dollars being measured, because calls differ in size by orders of
    /// magnitude.
    estimated_usage: Usage,
    /// Summed over locally-served turns: the cheapest frontier option the
    /// router had quoted at the moment it chose local.
    quoted_alternative_usd: f64,
}

impl Counters {
    /// Both provenances together, for the figures that are about volume rather
    /// than confidence.
    fn total_usage(&self) -> Usage {
        let mut total = self.reported_usage.clone();
        total.add(&self.estimated_usage);
        total
    }
}

/// A dispatch waiting for its response to terminate.
struct Pending {
    key: ModelKey,
    /// `None` when the chosen target was itself a frontier model, or when no
    /// frontier was quoted at all.
    best_frontier_alternative_usd: Option<f64>,
}

/// Folds session events into token and dollar aggregates.
///
/// Idempotent by `(session, seq)`: an event already folded is ignored. That is
/// what lets a live feed and a rebuild-from-log coexist without double
/// counting, which they otherwise would the first time a process replayed a
/// session it had already been watching.
#[derive(Default)]
pub struct MetricsFold {
    models: BTreeMap<ModelKey, Counters>,
    /// Highest sequence number folded per session.
    watermarks: HashMap<SessionId, u64>,
    pending: HashMap<ResponseId, Pending>,
    /// The response each open turn is currently on, and the inverse.
    ///
    /// Kept so an abandoned dispatch can be *retired* rather than waited on
    /// forever. A turn whose owner was fenced mid-dispatch never gets a
    /// terminal event — the settle seam's append is best-effort on exactly
    /// that path — but the client's retry re-admits the same `turn_id` under a
    /// fresh `ResponseId`, and a second `TurnStarted` for a turn is positive
    /// proof that the previous response was abandoned. That is a supersession
    /// rule rather than a heuristic, and it is driven entirely off log
    /// contents, so a live fold and a replay still agree.
    ///
    /// Both maps drain: at a terminal event, and at supersession. What they do
    /// not cover is a turn abandoned and then never retried, which stays until
    /// the process ends.
    response_of_turn: HashMap<TurnId, ResponseId>,
    turn_of_response: HashMap<ResponseId, TurnId>,
    turns: u64,
    first_at_ms: Option<u64>,
    last_at_ms: Option<u64>,
}

impl MetricsFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event. Returns whether it was new.
    pub fn apply(&mut self, event: &SessionEvent) -> bool {
        let watermark = self.watermarks.entry(event.session_id.clone()).or_default();
        if event.seq <= *watermark {
            return false;
        }
        *watermark = event.seq;

        // Timestamps come from every event, not only the ones that carry
        // tokens: the window a rate is computed over is wall-clock, and a
        // session that opened an hour ago and has yet to complete a turn has
        // still been running for an hour.
        self.first_at_ms = Some(
            self.first_at_ms
                .map_or(event.at_ms, |at| at.min(event.at_ms)),
        );
        self.last_at_ms = Some(
            self.last_at_ms
                .map_or(event.at_ms, |at| at.max(event.at_ms)),
        );

        match &event.kind {
            SessionEventKind::TurnStarted {
                turn_id,
                response_id,
            } => {
                self.turns += 1;
                // A second start for this turn means the first response will
                // never terminate. Retire it now rather than hold it forever.
                if let Some(abandoned) = self
                    .response_of_turn
                    .insert(turn_id.clone(), response_id.clone())
                {
                    self.pending.remove(&abandoned);
                    self.turn_of_response.remove(&abandoned);
                }
                self.turn_of_response
                    .insert(response_id.clone(), turn_id.clone());
            }
            SessionEventKind::Routed {
                response_id,
                decision,
            } => {
                self.pending.insert(
                    response_id.clone(),
                    Pending {
                        key: ModelKey::from_target(&decision.chosen),
                        best_frontier_alternative_usd: best_frontier_alternative(decision),
                    },
                );
            }
            SessionEventKind::ResponseCompleted { response_id, usage }
            | SessionEventKind::ResponseIncomplete {
                response_id, usage, ..
            } => {
                // Settled: this response is nobody's open turn any more.
                if let Some(turn_id) = self.turn_of_response.remove(response_id) {
                    self.response_of_turn.remove(&turn_id);
                }
                let Some(pending) = self.pending.remove(response_id) else {
                    return true;
                };
                // The same evidence rule the cache ledger uses, and for the
                // same reason. A completion always consumed tokens; an
                // incomplete only did if it reports billed input, because the
                // engine also terminates dispatches that failed before
                // anything reached the provider and those carry empty usage.
                // Counting one of those would add a call that never happened
                // to the denominator of every rate on the dashboard.
                let consumed = matches!(event.kind, SessionEventKind::ResponseCompleted { .. })
                    || usage.input_tokens > 0;
                if !consumed {
                    return true;
                }

                let counters = self.models.entry(pending.key).or_default();
                counters.calls += 1;
                match usage.accounting {
                    Accounting::Reported => counters.reported_usage.add(usage),
                    Accounting::Estimated => {
                        counters.estimated_calls += 1;
                        counters.estimated_usage.add(usage);
                    }
                }
                if let Some(alternative) = pending.best_frontier_alternative_usd {
                    counters.quoted_alternative_usd += alternative;
                }
            }
            SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ItemAppended { .. }
            | SessionEventKind::OutputTextDelta { .. }
            | SessionEventKind::TurnDeduplicated { .. }
            | SessionEventKind::Error { .. } => {}
        }
        true
    }

    /// Fold a batch, returning how many were new.
    pub fn extend<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent>) -> usize {
        events.into_iter().filter(|e| self.apply(e)).count()
    }

    pub fn sessions(&self) -> usize {
        self.watermarks.len()
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// Dispatches that were routed but whose response has not terminated.
    ///
    /// Observability for the size of the pending map, so a test can assert on
    /// what the fold is still holding rather than only on what it has counted.
    pub fn pending_dispatches(&self) -> usize {
        self.pending.len()
    }

    /// Turns admitted whose response has not terminated.
    ///
    /// Companion to [`Self::pending_dispatches`]: a turn appears here from its
    /// `TurnStarted` and leaves at its terminal event or when a retry
    /// supersedes it, so the two counts drain together.
    pub fn open_turns(&self) -> usize {
        self.response_of_turn.len()
    }

    /// Traffic shape per hosted model, for inferring correlaries.
    fn frontier_shapes(&self) -> HashMap<(String, String), TokenShape> {
        self.models
            .iter()
            .filter(|(key, _)| key.mode == ServingMode::Frontier)
            .filter_map(|(key, counters)| {
                TokenShape::from_rollup(&counters.total_usage(), counters.calls)
                    .map(|shape| ((key.provider.clone(), key.model.clone()), shape))
            })
            .collect()
    }
}

/// The cheapest hosted option the router passed over when it chose local.
///
/// Read off the decision's own `considered` list rather than recomputed, so it
/// reflects the ledger state and prices in force at that moment. `None` when
/// local did not win — there is no alternative to a frontier call that was
/// itself the frontier — or when no hosted model was quoted.
fn best_frontier_alternative(decision: &DecisionRecord) -> Option<f64> {
    if !decision.chosen.is_local() {
        return None;
    }
    decision
        .considered
        .iter()
        .filter(|candidate| !candidate.target.is_local())
        .map(|candidate| candidate.expected_cost_usd)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

/// Token counts for one grouping, split the way a reader asks about them.
///
/// `cached_input` and `reasoning` are components of `input` and `output`, not
/// additions to them — see [`Usage`] — so `total` is `input + output` and the
/// two detail fields are already inside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBreakdown {
    pub input: u64,
    pub cached_input: u64,
    pub uncached_input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total: u64,
}

impl TokenBreakdown {
    pub fn from_usage(usage: &Usage) -> Self {
        Self {
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            uncached_input: usage.uncached_input_tokens(),
            output: usage.output_tokens,
            reasoning: usage.reasoning_tokens,
            total: usage.total(),
        }
    }

    pub fn add(&mut self, other: &TokenBreakdown) {
        self.input += other.input;
        self.cached_input += other.cached_input;
        self.uncached_input += other.uncached_input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.total += other.total;
    }

    /// Cached share of the prompt, 0.0..=1.0.
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.input == 0 {
            0.0
        } else {
            self.cached_input as f64 / self.input as f64
        }
    }
}

/// How much of a grouping's accounting came from the provider.
///
/// A deployment whose clients never ask for usage — or whose gateway strips it
/// — sees this fall below 1.0, and every figure below it becomes partly our own
/// arithmetic rather than a provider's. Surfaced rather than buried because the
/// failure it describes is silent by nature: unreported usage folds in as zero
/// tokens for zero dollars, which on a hosted model is indistinguishable from a
/// saving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub calls: u64,
    pub reported_calls: u64,
    pub estimated_calls: u64,
    /// Billed tokens the provider counted.
    pub reported_tokens: u64,
    /// Billed tokens Roundhouse counted in its place.
    ///
    /// Present because the call-weighted ratio above is a poor proxy for it:
    /// one unreported 200k-token turn and one reported 2k-token turn is 50%
    /// coverage by calls and 1% by tokens, and it is the token figure that
    /// tracks the money.
    pub estimated_tokens: u64,
}

impl Coverage {
    pub fn reported_fraction(&self) -> f64 {
        if self.calls == 0 {
            1.0
        } else {
            self.reported_calls as f64 / self.calls as f64
        }
    }

    /// Share of billed tokens the provider counted, 0.0..=1.0.
    pub fn reported_token_fraction(&self) -> f64 {
        let total = self.reported_tokens + self.estimated_tokens;
        if total == 0 {
            1.0
        } else {
            self.reported_tokens as f64 / total as f64
        }
    }

    fn add(&mut self, other: &Coverage) {
        self.calls += other.calls;
        self.reported_calls += other.reported_calls;
        self.estimated_calls += other.estimated_calls;
        self.reported_tokens += other.reported_tokens;
        self.estimated_tokens += other.estimated_tokens;
    }
}

/// What a row's money means, which depends on where it ran.
///
/// One tagged value rather than a `mode` field beside two mutually exclusive
/// money fields that were each zero when the other applied. That shape let a
/// row claim to be local and to have been billed, and it made every consumer
/// re-derive which fields were meaningful from `mode` — the repeated
/// conditional the review objected to, which was a symptom rather than the
/// defect.
///
/// Flattened on the wire, so the serialized row still carries `mode` and its
/// money at the top level and consumers are unaffected. The difference is that
/// the fields which do not apply are now absent rather than zero, and a zero
/// that is really zero is no longer indistinguishable from one that is
/// "not applicable".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModelAccounting {
    /// Served by our own fleet: bills nothing, priced against a correlary.
    Local {
        /// What this traffic would have cost on its correlary. Zero when the
        /// correlary is [`Correlary::Unpriced`].
        shadow_usd: f64,
        correlary: Correlary,
    },
    /// Issued to an external endpoint: bills real money.
    Frontier {
        /// The sum of the two below.
        billed_usd: f64,
        /// Priced from counts the provider reported.
        billed_measured_usd: f64,
        /// Priced from our own tokenizer, because the provider reported
        /// nothing. Not smaller or larger than the truth — unknown, since a
        /// tokenizer mismatch cuts either way.
        billed_estimated_usd: f64,
        /// The discount the provider's own cache applied. Wholly measured: an
        /// unreported call carries no cache reads to guess at.
        cache_savings_usd: f64,
    },
}

/// One model's row.
#[derive(Debug, Clone, Serialize)]
pub struct ModelMetrics {
    pub provider: String,
    pub model: String,
    pub calls: u64,
    pub tokens: TokenBreakdown,
    pub coverage: Coverage,
    #[serde(flatten)]
    pub accounting: ModelAccounting,
}

impl ModelMetrics {
    pub fn mode(&self) -> ServingMode {
        match self.accounting {
            ModelAccounting::Local { .. } => ServingMode::Local,
            ModelAccounting::Frontier { .. } => ServingMode::Frontier,
        }
    }

    /// Money billed. Structurally zero for a local row.
    pub fn billed_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier { billed_usd, .. } => billed_usd,
        }
    }

    pub fn billed_measured_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier {
                billed_measured_usd,
                ..
            } => billed_measured_usd,
        }
    }

    pub fn billed_estimated_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier {
                billed_estimated_usd,
                ..
            } => billed_estimated_usd,
        }
    }

    /// What this would have cost hosted. Structurally zero for a hosted row,
    /// which is billed rather than shadow-priced.
    pub fn shadow_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { shadow_usd, .. } => shadow_usd,
            ModelAccounting::Frontier { .. } => 0.0,
        }
    }

    pub fn cache_savings_usd(&self) -> f64 {
        match self.accounting {
            ModelAccounting::Local { .. } => 0.0,
            ModelAccounting::Frontier {
                cache_savings_usd, ..
            } => cache_savings_usd,
        }
    }

    pub fn correlary(&self) -> Option<&Correlary> {
        match &self.accounting {
            ModelAccounting::Local { correlary, .. } => Some(correlary),
            ModelAccounting::Frontier { .. } => None,
        }
    }
}

/// A provider's rollup across its models.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderMetrics {
    pub provider: String,
    pub mode: ServingMode,
    pub calls: u64,
    pub tokens: TokenBreakdown,
    pub coverage: Coverage,
    pub billed_usd: f64,
    pub billed_measured_usd: f64,
    pub billed_estimated_usd: f64,
    pub shadow_usd: f64,
    pub cache_savings_usd: f64,
    pub models: usize,
}

/// A serving mode's rollup.
#[derive(Debug, Clone, Serialize)]
pub struct ServingModeMetrics {
    pub mode: ServingMode,
    pub calls: u64,
    pub tokens: TokenBreakdown,
    pub coverage: Coverage,
    pub billed_usd: f64,
    pub billed_measured_usd: f64,
    pub billed_estimated_usd: f64,
    pub shadow_usd: f64,
    pub cache_savings_usd: f64,
}

/// The headline, decomposed by how much each part can be trusted.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Savings {
    /// Money billed by hosted providers: `measured + estimated` below.
    ///
    /// Not labelled "measured" as a whole, which it was and which was wrong.
    /// A provider that reports no usage still bills, and the tokens standing in
    /// for its silence are ours, not its.
    pub frontier_spend_usd: f64,
    /// The part of `frontier_spend_usd` priced from provider-reported counts.
    pub frontier_spend_measured_usd: f64,
    /// The part priced from our own tokenizer, because the provider was silent.
    pub frontier_spend_estimated_usd: f64,
    /// Measured. The discount hosted caches applied to prompt tokens they had
    /// already seen.
    ///
    /// Wholly measured even when coverage is partial, and not by luck: an
    /// unreported call records `cached_input_tokens: 0` because nothing
    /// observable bears on what a remote cache did, so an estimated call
    /// contributes exactly zero here rather than a guess.
    pub cache_savings_usd: f64,
    /// Estimated. What local traffic would have cost on its correlary — a call
    /// that never happened, priced against a model chosen by [`pricing`].
    pub routing_savings_usd: f64,
    /// Estimated, independently. The same quantity as `routing_savings_usd`
    /// but taken from the router's own quotes at decision time rather than
    /// from a correlary.
    ///
    /// Kept as a cross-check, not added into the total. Two estimates of one
    /// counterfactual built from different inputs — one from a rate card and a
    /// similarity argument, one from the live cache ledger and the catalog the
    /// router was actually choosing from — should land near each other. When
    /// they do not, one of the two models is wrong, and that disagreement is
    /// worth more than either number alone.
    pub routing_savings_at_decision_usd: f64,
    /// `cache_savings_usd + routing_savings_usd`.
    pub total_usd: f64,
}

/// Everything the dashboard renders, at one instant.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub generated_at_ms: u64,
    pub first_event_at_ms: Option<u64>,
    pub last_event_at_ms: Option<u64>,
    pub sessions: usize,
    pub turns: u64,
    pub calls: u64,
    pub tokens: TokenBreakdown,
    pub savings: Savings,
    pub coverage: Coverage,
    /// Share of *calls* the provider accounted for.
    pub coverage_fraction: f64,
    /// Share of billed *tokens* the provider accounted for.
    ///
    /// The one to quote next to a dollar figure: spend tracks tokens, and a
    /// deployment can have most of its calls reported and most of its tokens
    /// not, or the reverse.
    pub coverage_token_fraction: f64,
    pub models: Vec<ModelMetrics>,
    pub providers: Vec<ProviderMetrics>,
    pub serving_modes: Vec<ServingModeMetrics>,
    /// The capability band the correlary inference was gated on, echoed so a
    /// reader can see how loose the comparison was allowed to be.
    pub capability_band: f64,
}

/// What the snapshot needs beyond the fold: rate cards, declared correlaries,
/// and the capability priors the gate compares against.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub pricing: ShadowPricing,
    /// Declared capability of each local model, keyed by model name.
    pub local_quality_priors: HashMap<String, f64>,
    /// Used for a local model with no entry above.
    pub default_local_quality_prior: f64,
}

impl MetricsConfig {
    pub fn new(pricing: ShadowPricing) -> Self {
        Self {
            pricing,
            local_quality_priors: HashMap::new(),
            default_local_quality_prior: 0.5,
        }
    }

    pub fn with_local_quality(mut self, model: impl Into<String>, prior: f64) -> Self {
        self.local_quality_priors.insert(model.into(), prior);
        self
    }

    pub fn with_default_local_quality(mut self, prior: f64) -> Self {
        self.default_local_quality_prior = prior;
        self
    }

    fn local_quality(&self, model: &str) -> f64 {
        self.local_quality_priors
            .get(model)
            .copied()
            .unwrap_or(self.default_local_quality_prior)
    }

    fn rate_card(&self, provider: &str, model: &str) -> Option<&ReferenceModel> {
        self.pricing
            .references()
            .iter()
            .find(|r| r.provider == provider && r.model == model)
    }
}

impl MetricsSnapshot {
    /// Apply a rate card to a fold.
    ///
    /// Separate from the fold on purpose: prices change, and a corrected rate
    /// card has to be able to reprice history without replaying it.
    pub fn build(fold: &MetricsFold, config: &MetricsConfig, generated_at_ms: u64) -> Self {
        let frontier_shapes = fold.frontier_shapes();

        let mut models = Vec::with_capacity(fold.models.len());
        for (key, counters) in &fold.models {
            let total_usage = counters.total_usage();
            let tokens = TokenBreakdown::from_usage(&total_usage);
            let coverage = Coverage {
                calls: counters.calls,
                reported_calls: counters.calls.saturating_sub(counters.estimated_calls),
                estimated_calls: counters.estimated_calls,
                reported_tokens: counters.reported_usage.total(),
                estimated_tokens: counters.estimated_usage.total(),
            };

            let accounting = match key.mode {
                ServingMode::Frontier => {
                    // A hosted model with no rate card bills an unknown amount,
                    // and zero is the wrong guess. It is reported as zero
                    // dollars against non-zero tokens, which is visible on the
                    // dashboard as a row that used tokens for free — the shape
                    // of a missing rate card rather than of a bargain.
                    let rate = config.rate_card(&key.provider, &key.model);
                    // Priced per provenance, which costs nothing extra because
                    // `price` is linear in tokens, and is the only way the two
                    // parts can be reported apart afterwards.
                    let billed = Billed {
                        measured: rate.map_or(0.0, |r| r.pricing.price(&counters.reported_usage)),
                        estimated: rate.map_or(0.0, |r| r.pricing.price(&counters.estimated_usage)),
                    };
                    // Wholly measured: an unreported call carries
                    // `cached_input_tokens: 0`, so it contributes nothing here
                    // rather than a guess.
                    ModelAccounting::Frontier {
                        billed_usd: billed.total(),
                        billed_measured_usd: billed.measured,
                        billed_estimated_usd: billed.estimated,
                        cache_savings_usd: rate
                            .map_or(0.0, |r| r.pricing.cache_savings(&total_usage)),
                    }
                }
                ServingMode::Local => {
                    let shape = TokenShape::from_rollup(&total_usage, counters.calls);
                    let correlary = config.pricing.resolve(
                        &key.model,
                        config.local_quality(&key.model),
                        shape,
                        &frontier_shapes,
                    );
                    ModelAccounting::Local {
                        shadow_usd: correlary.shadow_cost_usd(&total_usage),
                        correlary,
                    }
                }
            };

            models.push(ModelMetrics {
                provider: key.provider.clone(),
                model: key.model.clone(),
                calls: counters.calls,
                tokens,
                coverage,
                accounting,
            });
        }

        // Biggest spend first, then biggest shadow price: the row a reader
        // wants is almost always the expensive one, and a stable secondary key
        // keeps ordering from flickering between polls when spend ties at zero.
        models.sort_by(|a, b| {
            total_dollars(b)
                .partial_cmp(&total_dollars(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.tokens.total.cmp(&a.tokens.total))
                .then_with(|| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)))
        });

        let providers = roll_up_providers(&models);
        let serving_modes = roll_up_modes(&models);

        let mut tokens = TokenBreakdown::default();
        let mut coverage = Coverage::default();
        let mut calls = 0;
        for model in &models {
            tokens.add(&model.tokens);
            coverage.add(&model.coverage);
            calls += model.calls;
        }

        let cache_savings_usd = models.iter().map(|m| m.cache_savings_usd()).sum();
        let routing_savings_usd = models.iter().map(|m| m.shadow_usd()).sum();
        let savings = Savings {
            frontier_spend_usd: models.iter().map(|m| m.billed_usd()).sum(),
            frontier_spend_measured_usd: models.iter().map(|m| m.billed_measured_usd()).sum(),
            frontier_spend_estimated_usd: models.iter().map(|m| m.billed_estimated_usd()).sum(),
            cache_savings_usd,
            routing_savings_usd,
            routing_savings_at_decision_usd: fold
                .models
                .values()
                .map(|c| c.quoted_alternative_usd)
                .sum(),
            total_usd: cache_savings_usd + routing_savings_usd,
        };

        Self {
            generated_at_ms,
            first_event_at_ms: fold.first_at_ms,
            last_event_at_ms: fold.last_at_ms,
            sessions: fold.sessions(),
            turns: fold.turns(),
            calls,
            tokens,
            savings,
            coverage,
            coverage_fraction: coverage.reported_fraction(),
            coverage_token_fraction: coverage.reported_token_fraction(),
            models,
            providers,
            serving_modes,
            capability_band: config.pricing.capability_band(),
        }
    }
}

/// A billed figure, split by how its tokens were counted.
///
/// A two-field struct rather than a pair, because `(f64, f64)` at a call site
/// is exactly the shape that gets transposed once and never noticed.
#[derive(Debug, Clone, Copy, Default)]
struct Billed {
    measured: f64,
    estimated: f64,
}

impl Billed {
    fn total(&self) -> f64 {
        self.measured + self.estimated
    }
}

/// Ordering key for the model table: everything a row is worth, billed or not.
fn total_dollars(model: &ModelMetrics) -> f64 {
    model.billed_usd() + model.shadow_usd()
}

fn roll_up_providers(models: &[ModelMetrics]) -> Vec<ProviderMetrics> {
    let mut by_provider: BTreeMap<(ServingMode, String), ProviderMetrics> = BTreeMap::new();
    for model in models {
        let entry = by_provider
            .entry((model.mode(), model.provider.clone()))
            .or_insert_with(|| ProviderMetrics {
                provider: model.provider.clone(),
                mode: model.mode(),
                calls: 0,
                tokens: TokenBreakdown::default(),
                coverage: Coverage::default(),
                billed_usd: 0.0,
                billed_measured_usd: 0.0,
                billed_estimated_usd: 0.0,
                shadow_usd: 0.0,
                cache_savings_usd: 0.0,
                models: 0,
            });
        entry.calls += model.calls;
        entry.tokens.add(&model.tokens);
        entry.coverage.add(&model.coverage);
        entry.billed_usd += model.billed_usd();
        entry.billed_measured_usd += model.billed_measured_usd();
        entry.billed_estimated_usd += model.billed_estimated_usd();
        entry.shadow_usd += model.shadow_usd();
        entry.cache_savings_usd += model.cache_savings_usd();
        entry.models += 1;
    }
    let mut providers: Vec<_> = by_provider.into_values().collect();
    providers.sort_by(|a, b| {
        (b.billed_usd + b.shadow_usd)
            .partial_cmp(&(a.billed_usd + a.shadow_usd))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.provider.cmp(&b.provider))
    });
    providers
}

fn roll_up_modes(models: &[ModelMetrics]) -> Vec<ServingModeMetrics> {
    // Both modes are always present, even at zero calls. A dashboard that made
    // the local row vanish when nothing had been routed locally would show its
    // most alarming state — no local serving at all — as an empty space.
    let mut modes: Vec<ServingModeMetrics> = [ServingMode::Local, ServingMode::Frontier]
        .into_iter()
        .map(|mode| ServingModeMetrics {
            mode,
            calls: 0,
            tokens: TokenBreakdown::default(),
            coverage: Coverage::default(),
            billed_usd: 0.0,
            billed_measured_usd: 0.0,
            billed_estimated_usd: 0.0,
            shadow_usd: 0.0,
            cache_savings_usd: 0.0,
        })
        .collect();
    for model in models {
        let entry = modes
            .iter_mut()
            .find(|m| m.mode == model.mode())
            .expect("every serving mode has a row");
        entry.calls += model.calls;
        entry.tokens.add(&model.tokens);
        entry.coverage.add(&model.coverage);
        entry.billed_usd += model.billed_usd();
        entry.billed_measured_usd += model.billed_measured_usd();
        entry.billed_estimated_usd += model.billed_estimated_usd();
        entry.shadow_usd += model.shadow_usd();
        entry.cache_savings_usd += model.cache_savings_usd();
    }
    modes
}

// ---------------------------------------------------------------------------
// Live recording
// ---------------------------------------------------------------------------

/// Notified of every event a session commits.
///
/// Implemented by [`MetricsRecorder`], and deliberately narrow enough that
/// anything else wanting to watch the log — an exporter, a tracer — hangs off
/// the same seam instead of growing a second one.
///
/// Called while the session holds its lease and before the commit returns, so
/// an implementation must not block or await. The metrics fold is a few integer
/// additions, which is the budget.
pub trait SessionObserver: Send + Sync + 'static {
    fn observe(&self, events: &[SessionEvent]);
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
    use crate::event::{IncompleteReason, SessionEvent};
    use crate::ids::{ResponseId, SessionId, TurnId};
    use crate::routing::{Candidate, ProviderPricing};

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
        assert_eq!(local_mode.tokens.input, 20_000);
        assert_eq!(local_mode.billed_usd, 0.0, "local bills nothing");
        assert!(local_mode.shadow_usd > 0.0, "local is shadow-priced");

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
