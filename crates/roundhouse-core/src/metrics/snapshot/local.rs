// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A local row's money: its correlary, its shadow price and its capacity cost.
//!
//! Split out of `snapshot.rs` because the local arm is the one that carries
//! two answers — what the row publishes, and what part of its capacity spend
//! the routing saving offsets — and returning both from one function is what
//! lets `MetricsSnapshot::build` sum the offset without a loop-carried
//! accumulator beside the rows.

use std::collections::HashMap;

use super::{MetricsConfig, ModelAccounting, TokenBreakdown};
use crate::metrics::fold::Counters;
use crate::metrics::pricing::TokenShape;

/// A local row's accounting, and the capacity cost the routing saving nets.
pub(super) struct LocalRow {
    pub(super) accounting: ModelAccounting,
    /// The capacity cost of this row's priceable turns, when the catalog
    /// prices capacity *and* the row has a priced correlary; otherwise zero.
    ///
    /// Not a figure the row publishes: its `capacity_usd` is its whole
    /// capacity spend, a seat's share included, and this is the part of that
    /// spend the saving offsets.
    pub(super) routing_capacity_offset_usd: f64,
}

/// Price one local row.
pub(super) fn local_row(
    model: &str,
    counters: &Counters,
    config: &MetricsConfig,
    frontier_shapes: &HashMap<(String, String), TokenShape>,
) -> LocalRow {
    let total_usage = counters.total_usage();
    // The saving prices this and never `total_usage`: the seat's share of a
    // row is measured and has no saving to claim (see `Counters::seat`). The
    // capacity spend is the exception, over the whole row, because the
    // hardware is this deployment's whoever the caller is.
    let priceable = counters.billed.total();
    // The correlary is inferred from the *whole* row's shape and priced over
    // the priceable part of it. Two different questions: which hosted model
    // this traffic resembles is answered by the traffic, all of it, while what
    // it would have saved is answered only for the turns whose alternative
    // would have been this deployment's money.
    let shape = TokenShape::from_rollup(&total_usage, counters.calls);
    let correlary = config.pricing.resolve(
        model,
        config.local_quality(model),
        shape,
        frontier_shapes,
        // What this row's turns said they were talking to, where they agreed.
        // The counterfactual a client named is a better answer than one
        // inferred from traffic shape, and a worse one than a procurement
        // decision an operator wrote down — `resolve` holds that order.
        counters.declared_baseline.resolved(),
    );
    let capacity_price = config.local_capacity_price;
    // Netted only where there is a saving to net: a row with no priced
    // correlary claims none, so subtracting its capacity would publish a loss
    // against an alternative nobody could price.
    let routing_capacity_offset_usd = match capacity_price {
        Some(price) if correlary.reference().is_some() => price.price(priceable.tokens()),
        _ => 0.0,
    };
    LocalRow {
        accounting: ModelAccounting::Local {
            // Pooled like a hosted row's, though today no local dispatch
            // reports a cache write and every one of them takes the
            // conservative branch. Pricing the pot keeps the counterfactual
            // additive by construction rather than by that accident, so a
            // serving plane that starts reporting one does not quietly move a
            // published saving.
            shadow_usd: correlary.shadow_cost_pooled(&priceable),
            correlary,
            seat_tokens: TokenBreakdown::from_usage(counters.seat.total().tokens()),
            seat_estimated_calls: counters.seat_estimated_calls,
            capacity_usd: capacity_price.map(|price| price.price(&total_usage)),
        },
        routing_capacity_offset_usd,
    }
}
