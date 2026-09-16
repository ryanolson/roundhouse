// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The routing ledger: modelling a cache we cannot query.
//!
//! For local workers the selection service reports prefix overlap directly. For
//! a frontier provider there is no such query, so the ledger reconstructs the
//! answer from what we know: what we last sent to that target, how long ago,
//! and how that provider's cache expires.
//!
//! The append-only property of a session makes this tractable. Within one
//! session we send the whole conversation every turn, so whatever we sent to a
//! target last time is a *prefix* of what we are about to send. The expected
//! cached portion is therefore `p_hit(elapsed) * last_prefix_tokens`, and the
//! only hard part is `p_hit`.
//!
//! That property breaks if the context is compacted or truncated — dropping
//! early turns makes the old prompt no longer a prefix of the new one, and the
//! model would overestimate. [`CacheLedger::invalidate`] exists for that case
//! and must be called whenever the assembler rewrites history rather than
//! appending to it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::Usage;
use crate::routing::Target;

/// How a target's prefix cache expires.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CacheModel {
    /// Explicit breakpoints with a deterministic lifetime, refreshed on each
    /// hit. Anthropic's `cache_control` behaves this way, which makes it the
    /// easiest provider to route against: within the window a hit is a
    /// near-certainty rather than a hope.
    Deterministic { ttl_ms: u64 },

    /// Automatic caching above a minimum prefix length, evicted after a period
    /// of inactivity that is not contractually fixed. OpenAI behaves this way;
    /// a stable `prompt_cache_key` improves the odds by steering requests to
    /// the same cache node, which is why the executor always sends one.
    InactivityDecay {
        half_life_ms: u64,
        max_ttl_ms: u64,
        min_prefix_tokens: u64,
    },

    /// The router reports overlap directly, so no model is needed. Present so
    /// local targets can share the ledger's bookkeeping.
    Observed,
}

impl CacheModel {
    /// Probability that a prefix of `prefix_tokens` is still cached after
    /// `elapsed_ms`.
    pub fn hit_probability(&self, elapsed_ms: u64, prefix_tokens: u64) -> f64 {
        if prefix_tokens == 0 {
            return 0.0;
        }
        match *self {
            CacheModel::Deterministic { ttl_ms } => {
                if elapsed_ms < ttl_ms {
                    1.0
                } else {
                    0.0
                }
            }
            CacheModel::InactivityDecay {
                half_life_ms,
                max_ttl_ms,
                min_prefix_tokens,
            } => {
                if prefix_tokens < min_prefix_tokens || elapsed_ms >= max_ttl_ms {
                    return 0.0;
                }
                if half_life_ms == 0 {
                    return 0.0;
                }
                0.5f64.powf(elapsed_ms as f64 / half_life_ms as f64)
            }
            // Never guessed at; callers use the router's reported overlap.
            CacheModel::Observed => 0.0,
        }
    }
}

/// Per-million-token prices.
///
/// These are configuration, not constants: provider prices change, and baking
/// them into code guarantees they go stale. [`ProviderPricing::free`] is the
/// right default for local targets, whose marginal cost we account for in
/// prefill tokens rather than dollars.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProviderPricing {
    pub input_per_mtok_usd: f64,
    /// Reads against a warm prefix. The gap between this and
    /// `input_per_mtok_usd` is the entire economic lever this design pulls.
    pub cached_input_per_mtok_usd: f64,
    /// Writing a prefix into the cache, which some providers price at a
    /// premium over ordinary input.
    pub cache_write_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

impl ProviderPricing {
    pub fn free() -> Self {
        Self {
            input_per_mtok_usd: 0.0,
            cached_input_per_mtok_usd: 0.0,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: 0.0,
        }
    }

    /// The rate an uncached prompt token is billed at.
    ///
    /// Uncached prompt tokens are also *written* into the cache, so a provider
    /// that charges a premium for the write bills them at that rate rather
    /// than the plain input rate. A zero cache-write rate means the provider
    /// does not price the write separately, not that writes are free.
    pub fn effective_write_per_mtok_usd(&self) -> f64 {
        if self.cache_write_per_mtok_usd > 0.0 {
            self.cache_write_per_mtok_usd
        } else {
            self.input_per_mtok_usd
        }
    }

    /// Price a call from its four billable token axes.
    ///
    /// **The one definition of what a call costs.** Both the routing quote,
    /// which works in fractional expected tokens, and the metrics rollup, which
    /// works in measured integer counts, reach the arithmetic through here — a
    /// second copy would let the dashboard's "what we paid" drift from the
    /// router's "what we thought it would cost", and the gap between those two
    /// numbers is exactly what tells you the cache model is wrong.
    ///
    /// The two public entry points differ only in how they *split* the uncached
    /// prompt between `cache_write` and `plain_input`, never in what a token of
    /// each costs. That is the whole reason this takes four axes instead of
    /// three: it makes the conservative convention and the measured split two
    /// arguments to one formula rather than two formulas.
    fn price_axes(
        &self,
        cache_write: f64,
        cached_input: f64,
        plain_input: f64,
        output: f64,
    ) -> f64 {
        const PER_MTOK: f64 = 1e-6;
        cache_write * self.effective_write_per_mtok_usd() * PER_MTOK
            + cached_input * self.cached_input_per_mtok_usd * PER_MTOK
            + plain_input * self.input_per_mtok_usd * PER_MTOK
            + output * self.output_per_mtok_usd * PER_MTOK
    }

    /// Price a call from token counts nobody measured a cache write on.
    ///
    /// **The quote-time estimator, and it stays conservative on purpose.** It
    /// bills the *whole* uncached share at [`Self::effective_write_per_mtok_usd`]
    /// — the premium rate — because at quote time nothing knows how much of the
    /// prompt the provider will actually write into its cache, and D16's trade
    /// resolves the safe way: overstating our own cost understates the saving we
    /// claim, and a savings dashboard that errs must err downwards.
    ///
    /// M11.0 gave [`Self::price`] the measured split this cannot have. The two
    /// now differ on a turn whose provider reported a cache write, and that
    /// difference is a feature: the router's quote is a prediction and the
    /// rollup's price is a bill, and comparing them is how the cache model gets
    /// checked. Making this one measured-aware is not possible — there is no
    /// measurement yet — and making [`Self::price`] conservative would throw away
    /// one that exists.
    pub fn price_tokens(&self, uncached_input: f64, cached_input: f64, output: f64) -> f64 {
        self.price_axes(uncached_input, cached_input, 0.0, output)
    }

    /// How many of one call's uncached prompt tokens bill at the write rate.
    ///
    /// **The whole of what pricing decides per *call* rather than per token**,
    /// and therefore the one quantity a pot of calls cannot recover from its
    /// summed counts. Rate-card-free on purpose: this is a question about
    /// tokens, which is what lets [`PooledUsage`] take the decision at fold
    /// time without the fold ever seeing a price.
    ///
    /// **Where the measurement exists, it is used.** A provider that reports
    /// `cache_creation_input_tokens` has told us which uncached tokens carried
    /// the write premium and, by subtraction, which were ordinary input — so
    /// those two are billed at their two rates instead of all of them at the
    /// premium. That is the correction `ledger.rs` has carried as a known
    /// overcharge since M8 and could not make until `Usage` had somewhere to
    /// store the count.
    ///
    /// **A zero write count takes the conservative path, and that is not a
    /// rounding decision.** The log stores `0` both for "the provider reported
    /// no cache write" and for "this dialect reports no cache write at all", and
    /// nothing distinguishes them at this seam. Treating zero as measured would
    /// re-price every Responses turn ever recorded at the plain input rate —
    /// silently cutting our own recorded cost, which inflates the saving. So the
    /// measured split is taken only when there is a positive measurement to take
    /// it from.
    fn write_rate_tokens(usage: &Usage) -> u64 {
        let uncached = usage.uncached_input_tokens();
        // Clamped because the arithmetic in `price_pooled` must not produce a
        // negative `plain` share. On the Anthropic wire it cannot: the client
        // folds three disjoint counters, so `cache_creation` is inside
        // `input - cache_read` by construction. The clamp is against a *later*
        // dialect whose decoder gets the fold wrong, where the alternative is a
        // negative price that reads as a credit in every rollup downstream.
        let written = usage.cache_write_tokens.min(uncached);
        if written == 0 { uncached } else { written }
    }

    /// Price a measured call.
    ///
    /// Reasoning tokens are not added: they are already inside `output_tokens`,
    /// and every provider that reports them bills them as ordinary output.
    ///
    /// One call is a pot of one, spelled that way rather than duplicated:
    /// a second copy of the arithmetic is how the per-turn dollars a spend
    /// ledger commits come to disagree with the rollup's, which is the whole
    /// of what [`PooledUsage`] exists to prevent.
    pub fn price(&self, usage: &Usage) -> f64 {
        self.price_pooled(&PooledUsage::of(usage))
    }

    /// Price many calls whose write share was decided one call at a time.
    ///
    /// **The rollup's entry point, and where "a rollup's dollars are the sum of
    /// its calls' dollars" stops being an assumption.** Given a
    /// [`PooledUsage`], every axis below is a plain sum over the pot's calls and
    /// `price_axes` is linear in each, so this returns exactly what
    /// pricing those calls one at a time and adding the dollars would — for any
    /// mix of measured and unmeasured writes, which is what
    /// [`Self::price`] on a summed [`Usage`] cannot do and must not be asked to.
    pub fn price_pooled(&self, pooled: &PooledUsage) -> f64 {
        let uncached = pooled.tokens.uncached_input_tokens();
        // Already true by construction — every call contributes at most its own
        // uncached share — and asserted anyway because the subtraction below is
        // over `u64`: the failure of a broken invariant here would be a panic in
        // a dashboard poll rather than a wrong number.
        let at_write_rate = pooled.write_rate_tokens.min(uncached);
        self.price_axes(
            at_write_rate as f64,
            pooled.tokens.cached_input_tokens as f64,
            (uncached - at_write_rate) as f64,
            pooled.tokens.output_tokens as f64,
        )
    }

    /// What the cached portion of a call saved against paying full freight.
    ///
    /// Measured money, not a counterfactual about routing: these tokens were
    /// sent, the provider reported them as cache reads, and the difference
    /// between the two published rates is the discount it applied. The whole
    /// design exists to make this number large.
    pub fn cache_savings(&self, usage: &Usage) -> f64 {
        const PER_MTOK: f64 = 1e-6;
        let discount =
            (self.effective_write_per_mtok_usd() - self.cached_input_per_mtok_usd).max(0.0);
        usage.cached_input_tokens as f64 * discount * PER_MTOK
    }
}

/// Many calls' usage, pooled so that pricing the pot still costs what pricing
/// the calls did.
///
/// [`Usage::add`] sums every count a provider reports, and for three of the four
/// axes [`ProviderPricing::price_pooled`] bills, a sum is all a price needs. The
/// fourth is not a reported count at all: it is the decision
/// `ProviderPricing::write_rate_tokens` takes per call about how much of the
/// uncached prompt carries the cache-write premium, and the conservative branch
/// it takes when nothing measured a write makes that decision a fact about the
/// *call* rather than about its tokens. Sum two calls that disagree and the
/// decision is gone — the pot's `cache_write_tokens` says nothing about what the
/// unmeasured call was entitled to — so the pot prices for less than its calls
/// did, and the metrics rollup's `frontier_spend_usd` silently stops matching
/// the per-turn dollars the spend ledger commits. That divergence was M11.0
/// review finding F2, and it surfaced as a permanent phantom `drift_usd` in the
/// admin reconciliation view, whose three documented causes did not include a
/// pricing artifact.
///
/// So the decision is taken once, on the way in, and only its *result* is
/// accumulated. That makes "a rollup's dollars are the sum of its calls'
/// dollars" a property of this type rather than an assumption about `price`
/// being linear in tokens — the assumption M11.0's measured split retired.
///
/// **Holds no dollars, deliberately.** The split is a question about tokens, so
/// the metrics fold can accumulate one while staying money-free (see
/// `metrics::fold`, which keeps rate cards out of the fold so a corrected price
/// can reprice history without replaying it); only
/// [`ProviderPricing::price_pooled`] turns a pot into money.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PooledUsage {
    tokens: Usage,
    /// Uncached prompt tokens this pot bills at the cache-write rate, summed
    /// over each call's own decision and never re-derived from `tokens`.
    ///
    /// Private, and that is the enforcement: a consumer cannot price a pot by
    /// reaching past [`ProviderPricing::price_pooled`] for the tokens and
    /// splitting them itself, which is the shape of the defect this type
    /// replaces.
    write_rate_tokens: u64,
}

impl PooledUsage {
    /// A pot holding exactly one call.
    pub fn of(usage: &Usage) -> Self {
        let mut pooled = Self::default();
        pooled.add(usage);
        pooled
    }

    /// Book one call, taking its cache-write decision before it is pooled.
    pub fn add(&mut self, usage: &Usage) {
        self.write_rate_tokens += ProviderPricing::write_rate_tokens(usage);
        self.tokens.add(usage);
    }

    /// Merge another pot, its calls' decisions included.
    ///
    /// The reason a deployment-wide row can be derived from its tenants' rather
    /// than accumulated beside them: merging pots is exact, so there is nothing
    /// for the two to drift on.
    pub fn absorb(&mut self, other: &PooledUsage) {
        self.tokens.add(&other.tokens);
        self.write_rate_tokens += other.write_rate_tokens;
    }

    /// The tokens themselves, for every reader that asks about volume rather
    /// than money.
    pub fn tokens(&self) -> &Usage {
        &self.tokens
    }
}

/// What we last sent to a target.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TargetState {
    pub last_call_at_ms: u64,
    /// Prompt length of that call, and therefore the longest prefix that could
    /// still be warm.
    pub last_prefix_tokens: u64,
}

/// One recorded dispatch, projected from the session event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub target_key: String,
    pub at_ms: u64,
    pub isl_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// Per-session record of what went where, and the cache models to reason with.
#[derive(Debug, Clone, Default)]
pub struct CacheLedger {
    state: HashMap<String, TargetState>,
    models: HashMap<String, (CacheModel, ProviderPricing)>,
}

impl CacheLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register how a target's cache behaves and what it costs.
    ///
    /// Unregistered targets are treated as [`CacheModel::Observed`] and free,
    /// which is the correct fallback for local workers.
    pub fn register(&mut self, target: &Target, model: CacheModel, pricing: ProviderPricing) {
        self.models.insert(target.ledger_key(), (model, pricing));
    }

    pub fn model_for(&self, target: &Target) -> (CacheModel, ProviderPricing) {
        self.models
            .get(&target.ledger_key())
            .copied()
            .unwrap_or((CacheModel::Observed, ProviderPricing::free()))
    }

    /// Record a dispatch. Called as the session projects its event log.
    pub fn record(&mut self, target: &Target, at_ms: u64, isl_tokens: u64) {
        self.state.insert(
            target.ledger_key(),
            TargetState {
                last_call_at_ms: at_ms,
                last_prefix_tokens: isl_tokens,
            },
        );
    }

    /// Drop cached-prefix assumptions for every target.
    ///
    /// Required whenever the conversation stops being append-only — a
    /// compaction, a summarization, an edited history. Without this the model
    /// keeps claiming a warm prefix that no longer exists.
    pub fn invalidate(&mut self) {
        self.state.clear();
    }

    pub fn state_for(&self, target: &Target) -> Option<TargetState> {
        self.state.get(&target.ledger_key()).copied()
    }

    /// Expected number of prompt tokens served from cache.
    pub fn expected_cached_tokens(&self, target: &Target, now_ms: u64, isl_tokens: u64) -> f64 {
        let Some(state) = self.state_for(target) else {
            return 0.0;
        };
        // The warm prefix cannot exceed what we are about to send.
        let prefix = state.last_prefix_tokens.min(isl_tokens);
        let elapsed = now_ms.saturating_sub(state.last_call_at_ms);
        let (model, _) = self.model_for(target);
        model.hit_probability(elapsed, prefix) * prefix as f64
    }

    /// Expected dollar cost of one call to `target`.
    pub fn estimate_cost_usd(
        &self,
        target: &Target,
        now_ms: u64,
        isl_tokens: u64,
        expected_output_tokens: u64,
    ) -> f64 {
        let (_, pricing) = self.model_for(target);
        let cached = self.expected_cached_tokens(target, now_ms, isl_tokens);
        let uncached = (isl_tokens as f64 - cached).max(0.0);
        pricing.price_tokens(uncached, cached, expected_output_tokens as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Accounting;

    const MINUTE: u64 = 60_000;

    fn frontier(provider: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: "m".into(),
        }
    }

    /// A Claude-shaped rate card: the read is a tenth of input and the write is
    /// 1.25x it, which is the relationship the split below is about. All four
    /// figures differ, so a term billed at the wrong rate cannot cancel out.
    const CLAUDE: ProviderPricing = ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    };

    /// **A measured cache write is billed at the write rate and the rest of the
    /// uncached prompt at the input rate.**
    ///
    /// The overcharge `ledger.rs` has documented since M8: every uncached token
    /// was billed at `effective_write_per_mtok_usd` because nothing measured the
    /// write. On a provider that prices cache *creation* separately from
    /// ordinary uncached input — Anthropic's model, which is the one
    /// `CacheModel::Deterministic` was written for — that overstates the bill by
    /// the premium on every token that was never written.
    #[test]
    fn a_measured_cache_write_is_priced_apart_from_the_uncached_input_beside_it() {
        // 1M input of which 600k read from cache, 100k newly written, and so
        // 300k ordinary uncached prompt.
        let measured = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 600_000,
            cache_write_tokens: 100_000,
            output_tokens: 0,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        };
        // 0.1 M * 3.75 + 0.6 M * 0.3 + 0.3 M * 3.0 = 0.375 + 0.18 + 0.90
        assert!((CLAUDE.price(&measured) - 1.455).abs() < 1e-9);

        // And it is strictly cheaper than the convention it replaces, which is
        // the direction that matters: the correction can only ever *lower* a
        // recorded cost, so a deployment's committed spend does not grow when
        // this lands.
        let conventional = CLAUDE.price_tokens(400_000.0, 600_000.0, 0.0);
        assert!((conventional - 1.68).abs() < 1e-9);
        assert!(CLAUDE.price(&measured) < conventional);
    }

    /// **A zero write count is not a measurement, and takes the conservative
    /// path.**
    ///
    /// The log stores `0` both for "this provider reported no cache write" and
    /// for "this dialect has no such counter", and nothing at this seam tells
    /// them apart. Reading zero as a measurement would re-price every Responses
    /// turn ever recorded at the plain input rate — cutting our own recorded
    /// cost, which *inflates* the saving, which is the one direction this
    /// codebase refuses to err in.
    #[test]
    fn an_unmeasured_call_still_bills_every_uncached_token_at_the_write_rate() {
        let unmeasured = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 600_000,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        };
        // 0.4 M * 3.75 + 0.6 M * 0.3 — unchanged from before M11.0.
        assert!((CLAUDE.price(&unmeasured) - 1.68).abs() < 1e-9);
        assert!(
            (CLAUDE.price(&unmeasured) - CLAUDE.price_tokens(400_000.0, 600_000.0, 0.0)).abs()
                < 1e-9,
            "the two entry points must still agree on a call with no measurement, or the \
             router's quote and the rollup's bill diverge for a reason nobody chose"
        );

        // CONTROL: one field different — a single measured write token — and
        // the price moves. Without this the assertion above would also pass on
        // a build that had lost the measured split entirely.
        let barely = Usage {
            cache_write_tokens: 1,
            ..unmeasured.clone()
        };
        assert!(CLAUDE.price(&barely) < CLAUDE.price(&unmeasured));
    }

    /// A decoder that reported more cache creation than there was uncached
    /// prompt must not produce a negative bill.
    ///
    /// Unreachable on the Anthropic wire — the client folds three disjoint
    /// counters, so the write is inside `input - cache_read` by construction —
    /// and asserted anyway, because the failure mode is a *credit* appearing in
    /// every rollup downstream rather than an error anyone would see.
    #[test]
    fn an_impossible_write_count_clamps_rather_than_paying_us_back() {
        let broken = Usage {
            input_tokens: 1_000,
            cached_input_tokens: 900,
            // 900 read + 500 written is more input than there was.
            cache_write_tokens: 500,
            output_tokens: 0,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        };
        let price = CLAUDE.price(&broken);
        assert!(price > 0.0, "a call cannot cost less than nothing: {price}");
        // The whole uncached remainder at the write rate, and nothing negative
        // beside it: 100 * 3.75 + 900 * 0.3, per million.
        assert!((price - (100.0 * 3.75 + 900.0 * 0.3) * 1e-6).abs() < 1e-12);
    }

    /// **A pot costs what its calls cost, however they disagree.**
    ///
    /// The invariant the metrics rollup rests on, and the one a summed [`Usage`]
    /// cannot provide: an unmeasured call bills its whole uncached share at the
    /// write rate, a measured one splits, and a sum of the two counts has no way
    /// to say which tokens were which. Asserted over a deliberately mixed pot,
    /// and over a merge of two pots, because [`Counters::absorb`] in the metrics
    /// fold derives every deployment-wide row that way.
    ///
    /// [`Counters::absorb`]: crate::metrics
    #[test]
    fn a_pot_of_calls_prices_exactly_what_pricing_them_one_at_a_time_does() {
        let call = |uncached: u64, write: u64| Usage {
            input_tokens: uncached,
            cached_input_tokens: 0,
            cache_write_tokens: write,
            output_tokens: 0,
            reasoning_tokens: 0,
            accounting: Accounting::Reported,
        };
        // A short prompt under the cacheable minimum (nothing measured), a long
        // one written whole, and one that was partly written — all three
        // branches of the split in one pot.
        let calls = [call(1_000, 0), call(2_000, 2_000), call(4_000, 1_000)];

        let one_at_a_time: f64 = calls.iter().map(|usage| CLAUDE.price(usage)).sum();
        let mut pooled = PooledUsage::default();
        for usage in &calls {
            pooled.add(usage);
        }
        assert!(
            (CLAUDE.price_pooled(&pooled) - one_at_a_time).abs() < 1e-12,
            "pot = {}, one at a time = {one_at_a_time}",
            CLAUDE.price_pooled(&pooled),
        );

        // Merging pots is the same arithmetic, which is what lets a
        // deployment-wide row be derived from its tenants' rather than
        // accumulated beside them.
        let mut first = PooledUsage::of(&calls[0]);
        let mut second = PooledUsage::of(&calls[1]);
        second.add(&calls[2]);
        first.absorb(&second);
        assert_eq!(first, pooled);

        // CONTROL: the summed-`Usage` route this replaced still disagrees, and
        // by a figure large enough that the assertion above is not a tolerance
        // artifact. It is also the guard on the conservative unmeasured branch:
        // if this ever agrees, that branch has been flattened and every
        // unmeasured turn on record has been re-priced downwards.
        let mut summed = calls[0].clone();
        summed.add(&calls[1]);
        summed.add(&calls[2]);
        assert!(
            one_at_a_time - CLAUDE.price(&summed) > 1e-6,
            "summing before pricing must still understate: {one_at_a_time} vs {}",
            CLAUDE.price(&summed),
        );
    }

    #[test]
    fn deterministic_cache_is_a_cliff_at_the_ttl() {
        let model = CacheModel::Deterministic { ttl_ms: 5 * MINUTE };
        assert_eq!(model.hit_probability(0, 1000), 1.0);
        assert_eq!(model.hit_probability(5 * MINUTE - 1, 1000), 1.0);
        assert_eq!(model.hit_probability(5 * MINUTE, 1000), 0.0);
    }

    #[test]
    fn inactivity_decay_falls_off_and_respects_the_minimum_prefix() {
        let model = CacheModel::InactivityDecay {
            half_life_ms: 5 * MINUTE,
            max_ttl_ms: 10 * MINUTE,
            min_prefix_tokens: 1024,
        };
        assert!((model.hit_probability(0, 2048) - 1.0).abs() < 1e-9);
        assert!((model.hit_probability(5 * MINUTE, 2048) - 0.5).abs() < 1e-9);
        assert_eq!(model.hit_probability(10 * MINUTE, 2048), 0.0);
        // Below the provider's minimum cacheable prefix nothing is cached.
        assert_eq!(model.hit_probability(0, 512), 0.0);
    }

    #[test]
    fn an_unseen_target_has_no_warm_prefix() {
        let ledger = CacheLedger::new();
        assert_eq!(
            ledger.expected_cached_tokens(&frontier("anthropic"), 0, 5_000),
            0.0
        );
    }

    #[test]
    fn a_warm_prefix_is_capped_by_the_current_prompt_length() {
        let mut ledger = CacheLedger::new();
        let target = frontier("anthropic");
        ledger.register(
            &target,
            CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
            ProviderPricing::free(),
        );
        ledger.record(&target, 0, 4_000);

        // Prompt shrank below what we last sent; only the overlap can be warm.
        assert_eq!(
            ledger.expected_cached_tokens(&target, MINUTE, 1_000),
            1_000.0
        );
        // Prompt grew; the warm part is still the earlier prefix.
        assert_eq!(
            ledger.expected_cached_tokens(&target, MINUTE, 9_000),
            4_000.0
        );
    }

    #[test]
    fn a_cold_prefix_costs_full_price_and_a_warm_one_costs_the_cached_rate() {
        let mut ledger = CacheLedger::new();
        let target = frontier("anthropic");
        let pricing = ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        };
        ledger.register(
            &target,
            CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
            pricing,
        );

        // Never seen: the whole prompt is a cache write.
        let cold = ledger.estimate_cost_usd(&target, 0, 100_000, 500);
        assert!((cold - (100_000.0 * 3.75e-6 + 500.0 * 15e-6)).abs() < 1e-9);

        // Seen a minute ago with a 100k prefix: reads at the cached rate.
        ledger.record(&target, 0, 100_000);
        let warm = ledger.estimate_cost_usd(&target, MINUTE, 100_000, 500);
        assert!((warm - (100_000.0 * 0.3e-6 + 500.0 * 15e-6)).abs() < 1e-9);
        assert!(warm < cold, "a warm prefix must be cheaper than a cold one");
    }

    #[test]
    fn invalidation_clears_warm_prefixes_after_a_compaction() {
        let mut ledger = CacheLedger::new();
        let target = frontier("anthropic");
        ledger.register(
            &target,
            CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
            ProviderPricing::free(),
        );
        ledger.record(&target, 0, 50_000);
        assert!(ledger.expected_cached_tokens(&target, MINUTE, 50_000) > 0.0);

        ledger.invalidate();
        assert_eq!(ledger.expected_cached_tokens(&target, MINUTE, 50_000), 0.0);
    }

    #[test]
    fn expiry_returns_the_target_to_cold_pricing() {
        let mut ledger = CacheLedger::new();
        let target = frontier("anthropic");
        ledger.register(
            &target,
            CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
            ProviderPricing::free(),
        );
        ledger.record(&target, 0, 10_000);

        assert_eq!(
            ledger.expected_cached_tokens(&target, 4 * MINUTE, 10_000),
            10_000.0
        );
        assert_eq!(
            ledger.expected_cached_tokens(&target, 6 * MINUTE, 10_000),
            0.0
        );
    }
}
