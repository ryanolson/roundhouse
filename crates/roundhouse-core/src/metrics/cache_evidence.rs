// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Predicted and observed cache reuse for a settled dispatch.
//!
//! Only explicit provider cache counts supply observations, including zero.
//! Missing counts remain unknown. Locally derived counts cannot validate the
//! prediction that produced them. See [`CacheReadSource`](crate::event::CacheReadSource).

use crate::event::{Accounting, Usage};

/// Predicted against observed cache reuse, for one model row.
///
/// Sums and counts rather than means: rows merge, sums add exactly, and a mean
/// of means weights a row that served three turns like one that served three
/// hundred.
///
/// **Not split billed/seat**, unlike every token-touching field on `Counters`.
/// That split keeps a rate card off a seat's tokens; there is no rate card
/// here, and what a prompt's cache did is a fact about the prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct CacheEvidence {
    /// Summed predicted ratios over [`Self::paired`].
    pub(super) predicted_total: f64,
    /// Summed observed ratios over the same terminals.
    pub(super) observed_total: f64,
    /// Terminals carrying both a usable prediction and a stated cache read.
    pub(super) paired: u64,
    /// Terminals whose decision carried a prediction that can be divided.
    pub(super) predictions: u64,
    /// Terminals whose decision predicted nothing that can be divided.
    ///
    /// A zero `isl_tokens`, or an expected prefill that is not a finite
    /// non-negative count. Explicit rather than left to arithmetic, which is
    /// wrong in both directions: `f64::NAN.max(0.0)` is `0.0` in Rust, so a
    /// non-finite prefill reads as a confident "no reuse expected", and a
    /// negative one reads as a predicted 1.0.
    pub(super) unusable_prediction: u64,
    /// Terminals where the provider stated its cache read, zero included.
    pub(super) measured_cache_reads: u64,
    /// Terminals where nobody stated one: an omitted, null or unparseable
    /// field, or our own derived local credit.
    pub(super) unverifiable_cache_read: u64,
    /// Terminals reporting more cached input than input.
    ///
    /// Its own count rather than clamped to a 1.0: a provider that is
    /// miscounting is worth seeing, and a clamp publishes it as a perfect
    /// cache.
    pub(super) invalid_usage: u64,
    /// Terminals whose provider count could not be read at all: no provider
    /// accounting, or a reported zero input.
    pub(super) unusable_usage: u64,
}

impl CacheEvidence {
    /// Fold one settled dispatch's prediction against its evidence.
    ///
    /// The two censuses are taken independently — each partitions the terminals
    /// this row observed on its own — and a pair is booked only where both came
    /// out usable. A turn whose provider went silent still made a prediction
    /// worth recording.
    pub(super) fn observe(&mut self, isl_tokens: u64, expected_prefill_tokens: f64, usage: &Usage) {
        let predicted = predicted_ratio(isl_tokens, expected_prefill_tokens);
        match predicted {
            Some(_) => self.predictions += 1,
            None => self.unusable_prediction += 1,
        }

        let observed = match observed_ratio(usage) {
            Evidence::Measured(ratio) => {
                self.measured_cache_reads += 1;
                Some(ratio)
            }
            Evidence::Unverifiable => {
                self.unverifiable_cache_read += 1;
                None
            }
            Evidence::Invalid => {
                self.invalid_usage += 1;
                None
            }
            Evidence::Unusable => {
                self.unusable_usage += 1;
                None
            }
        };

        if let (Some(predicted), Some(observed)) = (predicted, observed) {
            self.predicted_total += predicted;
            self.observed_total += observed;
            self.paired += 1;
        }
    }

    pub(super) fn absorb(&mut self, other: &CacheEvidence) {
        self.predicted_total += other.predicted_total;
        self.observed_total += other.observed_total;
        self.paired += other.paired;
        self.predictions += other.predictions;
        self.unusable_prediction += other.unusable_prediction;
        self.measured_cache_reads += other.measured_cache_reads;
        self.unverifiable_cache_read += other.unverifiable_cache_read;
        self.invalid_usage += other.invalid_usage;
        self.unusable_usage += other.unusable_usage;
    }

    /// Terminals this row observed, however they were classified.
    ///
    /// Off the prediction census, which counts every one of them.
    pub(super) fn observed_terminals(&self) -> u64 {
        self.predictions + self.unusable_prediction
    }
}

/// What a terminal's usage says about the provider's cache.
enum Evidence {
    /// The provider stated its read, and it is consistent with its own input.
    Measured(f64),
    /// Nobody stated one. See [`CacheReadSource`](crate::event::CacheReadSource).
    Unverifiable,
    /// More cached input than input.
    Invalid,
    /// No provider accounting at all, or a reported zero input.
    Unusable,
}

/// The share of the prompt the decision expected to come from cache.
///
/// `None` where the record predicts nothing divisible. The guard is what makes
/// `max` below safe: it runs only on a finite non-negative prefill.
fn predicted_ratio(isl_tokens: u64, expected_prefill_tokens: f64) -> Option<f64> {
    if isl_tokens == 0 || !expected_prefill_tokens.is_finite() || expected_prefill_tokens < 0.0 {
        return None;
    }
    let isl = isl_tokens as f64;
    // The definition `Candidate::cache_hit_ratio` already gives, read off the
    // persisted record rather than recomputed from a live quote: the quote is
    // gone by the time this runs, and replay must reach the same answer from
    // the log alone.
    Some(((isl - expected_prefill_tokens).max(0.0) / isl).clamp(0.0, 1.0))
}

/// The share of the prompt the provider says it served from cache.
fn observed_ratio(usage: &Usage) -> Evidence {
    if usage.accounting != Accounting::Reported || usage.input_tokens == 0 {
        return Evidence::Unusable;
    }
    if !usage.cache_read_source.is_measured() {
        return Evidence::Unverifiable;
    }
    if usage.cached_input_tokens > usage.input_tokens {
        return Evidence::Invalid;
    }
    Evidence::Measured(usage.cached_input_tokens as f64 / usage.input_tokens as f64)
}
