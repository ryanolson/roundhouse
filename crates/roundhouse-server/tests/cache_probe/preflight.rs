// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The pure check a live run must pass before it spends anything.

use roundhouse_fleet::{FrontierModelSpec, WireProtocol};

/// Everything the live run must be true before it spends anything.
///
/// A pure function of the ceiling and the catalog entry, so the refusals are
/// testable without setting a variable in this process: `preflight` reads the
/// three settings and hands them here.
pub(super) fn probe_inputs(limit_usd: f64, spec: &FrontierModelSpec) -> Result<(), String> {
    if spec.wire_protocol != WireProtocol::AnthropicMessages {
        return Err(format!(
            "{}/{} speaks {:?}; `cache_control` is the Messages wire's vocabulary",
            spec.provider, spec.model, spec.wire_protocol
        ));
    }
    // Finite as well as positive: an infinite or NaN ceiling compares false
    // against every hold, so it would read as "plenty of room" forever.
    if !limit_usd.is_finite() || limit_usd <= 0.0 {
        return Err(format!(
            "a ceiling of {limit_usd} bounds no spend; give a finite, positive figure"
        ));
    }
    // All four prices, because this probe's economics run through all four: a
    // zero input or output price takes a zero hold and the ceiling stops
    // meaning anything, while a zero cache-write or cached-input price makes
    // the very discount under test free — which is the figure a reader would
    // take the run's word for.
    for (field, rate) in [
        ("input_per_mtok_usd", spec.pricing.input_per_mtok_usd),
        ("output_per_mtok_usd", spec.pricing.output_per_mtok_usd),
        (
            "cache_write_per_mtok_usd",
            spec.pricing.cache_write_per_mtok_usd,
        ),
        (
            "cached_input_per_mtok_usd",
            spec.pricing.cached_input_per_mtok_usd,
        ),
    ] {
        if rate <= 0.0 {
            return Err(format!(
                "{}/{} has {field} = {rate}; a spend-bounded probe needs the real rate card, \
                 and this dialect prices the cache on every one of the four",
                spec.provider, spec.model
            ));
        }
    }
    Ok(())
}
