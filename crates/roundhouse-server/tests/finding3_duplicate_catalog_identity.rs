// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Validation of review finding F3: the catalog is not a validated boundary.
//!
//! The stated invariant, from `catalog_config`'s own module docs and from
//! `StaticFrontierCatalog::shadow_pricing`: "the price the router optimizes
//! against and the price the dashboard reports saving must be the same number
//! or neither means anything". These tests ask whether one configuration file
//! can make those two numbers differ, and whether obviously invalid numbers
//! survive parsing.
//!
//! Validation only — no fix is applied here.

use roundhouse_core::event::{Accounting, SessionEvent, SessionEventKind, Usage};
use roundhouse_core::ids::{ResponseId, SessionId};
use roundhouse_core::metrics::{MetricsFold, MetricsSnapshot};
use roundhouse_core::routing::{CacheLedger, DecisionRecord, Target};
use roundhouse_server::CatalogConfig;

/// One catalog file, two entries for the same `(provider, model)`, different
/// prices. Nothing else about the two entries differs.
const DUPLICATE_IDENTITY: &str = r#"{
  "models": [
    {
      "provider": "anthropic",
      "model": "claude-sonnet",
      "wire_protocol": "anthropic_messages",
      "cache_model": { "kind": "deterministic", "ttl_ms": 300000 },
      "pricing": {
        "input_per_mtok_usd": 3.0,
        "cached_input_per_mtok_usd": 0.3,
        "cache_write_per_mtok_usd": 3.75,
        "output_per_mtok_usd": 15.0
      },
      "quality_prior": 0.62,
      "base_ttft_ms": 350.0,
      "ttft_ms_per_uncached_token": 0.002
    },
    {
      "provider": "anthropic",
      "model": "claude-sonnet",
      "wire_protocol": "anthropic_messages",
      "cache_model": { "kind": "deterministic", "ttl_ms": 300000 },
      "pricing": {
        "input_per_mtok_usd": 30.0,
        "cached_input_per_mtok_usd": 3.0,
        "cache_write_per_mtok_usd": 37.5,
        "output_per_mtok_usd": 150.0
      },
      "quality_prior": 0.62,
      "base_ttft_ms": 350.0,
      "ttft_ms_per_uncached_token": 0.002
    }
  ]
}"#;

fn target() -> Target {
    Target::Frontier {
        provider: "anthropic".into(),
        model: "claude-sonnet".into(),
    }
}

/// Exactly one million uncached prompt tokens, so a billed figure in dollars
/// reads back as the per-mtok rate that produced it.
fn one_mtok_of_uncached_input() -> Usage {
    Usage {
        input_tokens: 1_000_000,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

/// A log with a single frontier call against `target()`, for the fold.
fn one_frontier_call(usage: Usage) -> Vec<SessionEvent> {
    let session = SessionId::new("sess_dup");
    let response = ResponseId::new("resp_dup");
    vec![
        SessionEvent {
            seq: 1,
            session_id: session.clone(),
            at_ms: 1_000,
            kind: SessionEventKind::Routed {
                response_id: response.clone(),
                decision: DecisionRecord {
                    chosen: target(),
                    rationale: "test".into(),
                    policy: "test".into(),
                    isl_tokens: 1_000_000,
                    expected_prefill_tokens: 1_000_000.0,
                    expected_cost_usd: 0.0,
                    considered: Vec::new(),
                },
            },
        },
        SessionEvent {
            seq: 2,
            session_id: session,
            at_ms: 2_000,
            kind: SessionEventKind::ResponseCompleted {
                response_id: response,
                usage,
            },
        },
    ]
}

/// The headline claim: one config file, two prices for one model identity, and
/// the router and the dashboard end up on opposite sides of it.
#[test]
#[ignore = "F3: validated defect, unfixed — the catalog is not a validated boundary"]
fn duplicate_identity_splits_the_router_price_from_the_dashboard_price() {
    let config = CatalogConfig::from_json(DUPLICATE_IDENTITY, "duplicate.json")
        .expect("parsing accepts a duplicated model identity");
    assert_eq!(
        config.catalog().models().len(),
        2,
        "precondition: both entries survived parsing as distinct catalog rows"
    );

    // Router side: the ledger is seeded from the catalog, and a routing quote
    // is priced from whatever it holds for this target.
    let mut ledger = CacheLedger::new();
    config.catalog().apply_to_ledger(&mut ledger);
    let (_model, router_pricing) = ledger.model_for(&target());
    let router_price = router_pricing.price(&one_mtok_of_uncached_input());

    // Dashboard side: the same config's metrics config, applied to a fold that
    // recorded one call to that same target.
    let mut fold = MetricsFold::new();
    fold.extend(&one_frontier_call(one_mtok_of_uncached_input()));
    let snapshot = MetricsSnapshot::build(&fold, &config.metrics_config(), 3_000);
    assert_eq!(
        snapshot.models.len(),
        1,
        "the fold keys by model identity, so the duplicate is invisible here"
    );
    let dashboard_price = snapshot.models[0].billed_usd;

    assert_eq!(
        router_price, dashboard_price,
        "one catalog file, one model identity, one million uncached prompt \
         tokens: the router priced this call at ${router_price} and the \
         dashboard billed it at ${dashboard_price}. The stated invariant is \
         that these are the same number."
    );
}

/// Sub-claim: are negative prices accepted?
#[test]
#[ignore = "F3: validated defect, unfixed — the catalog is not a validated boundary"]
fn a_negative_price_is_refused_by_the_catalog_boundary() {
    let json = DUPLICATE_IDENTITY
        .replace("\"input_per_mtok_usd\": 3.0", "\"input_per_mtok_usd\": -3.0")
        .replace(
            "\"cache_write_per_mtok_usd\": 3.75",
            "\"cache_write_per_mtok_usd\": -3.75",
        )
        .replace(
            "\"output_per_mtok_usd\": 15.0",
            "\"output_per_mtok_usd\": -15.0",
        );
    let parsed = CatalogConfig::from_json(&json, "negative.json");

    // Show the consequence before ruling on the acceptance, so the failure
    // message carries what a negative rate actually does.
    if let Ok(config) = &parsed {
        let mut fold = MetricsFold::new();
        fold.extend(&one_frontier_call(one_mtok_of_uncached_input()));
        let snapshot = MetricsSnapshot::build(&fold, &config.metrics_config(), 3_000);
        let billed = snapshot.models[0].billed_usd;
        assert!(
            billed >= 0.0,
            "a negative rate card bills a real call at ${billed}, i.e. the \
             dashboard reports the fleet was paid to serve traffic"
        );
    }

    assert!(
        parsed.is_err(),
        "a catalog with negative per-mtok prices parsed successfully"
    );
}

/// Sub-claim: are non-finite prices accepted?
///
/// They are not, and not because the catalog checks: `serde_json` refuses both
/// `NaN` (no such JSON literal) and a float literal it cannot represent. This
/// half of the "invalid numeric ranges" claim is unreachable through the config
/// format. Kept as a passing test so the reason is on the record.
#[test]
fn a_nonfinite_price_cannot_be_expressed_in_the_config_format() {
    let overflow =
        DUPLICATE_IDENTITY.replace("\"input_per_mtok_usd\": 3.0", "\"input_per_mtok_usd\": 1e400");
    let error = CatalogConfig::from_json(&overflow, "overflow.json")
        .expect_err("serde_json refuses a float literal it cannot represent");
    assert!(error.to_string().contains("overflow.json"));

    let nan = DUPLICATE_IDENTITY.replace("\"input_per_mtok_usd\": 3.0", "\"input_per_mtok_usd\": NaN");
    assert!(CatalogConfig::from_json(&nan, "nan.json").is_err());
}

/// Sub-claim: is an out-of-range `quality_prior` accepted?
///
/// It is documented as 0.0..=1.0 and it gates the capability comparison, so a
/// value outside that range silently widens or closes the gate.
#[test]
#[ignore = "F3: validated defect, unfixed — the catalog is not a validated boundary"]
fn an_out_of_range_quality_prior_is_refused() {
    let prior = DUPLICATE_IDENTITY.replace("\"quality_prior\": 0.62", "\"quality_prior\": 42.0");
    let parsed = CatalogConfig::from_json(&prior, "prior.json");
    if let Ok(config) = &parsed {
        let observed = config.catalog().models()[0].quality_prior;
        assert!(
            (0.0..=1.0).contains(&observed),
            "the catalog carries a quality_prior of {observed}, outside the \
             documented 0.0..=1.0 the capability gate compares against"
        );
    }
    assert!(parsed.is_err(), "a quality_prior of 42.0 parsed successfully");
}

/// Sub-claim: is the `cache_write_per_mtok_usd == 0` sentinel ambiguous?
///
/// The documented reading is "the provider does not price the write
/// separately", and the fallback bills uncached input at the plain input rate.
/// A provider whose cache writes are genuinely free bills uncached prompt
/// tokens at the input rate too -- the write rides on tokens that are already
/// billed as input -- so both readings produce the same dollars.
#[test]
fn a_zero_cache_write_rate_bills_uncached_input_at_the_input_rate() {
    let zero_write = DUPLICATE_IDENTITY.replace(
        "\"cache_write_per_mtok_usd\": 3.75",
        "\"cache_write_per_mtok_usd\": 0.0",
    );
    let config = CatalogConfig::from_json(&zero_write, "zero_write.json").unwrap();
    let catalog = config.catalog();
    let spec = &catalog.models()[0];
    assert_eq!(spec.pricing.cache_write_per_mtok_usd, 0.0);
    assert_eq!(
        spec.pricing.effective_write_per_mtok_usd(),
        spec.pricing.input_per_mtok_usd,
        "a zero write rate falls back to the input rate"
    );
    // And the fallback is not reachable by a provider that means "free": a
    // write happens on tokens already billed as input, so zero-extra and
    // no-separate-charge are the same dollars.
    assert_eq!(
        spec.pricing.price_tokens(1_000_000.0, 0.0, 0.0),
        spec.pricing.input_per_mtok_usd * 1e-6 * 1_000_000.0
    );
}
