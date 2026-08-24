// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review finding F5: `Correlary` and `ModelMetrics` accepted states their own
//! documentation calls impossible.
//!
//! They did, through the public API and through serde. Nothing in the tree
//! constructed one — `ShadowPricing::resolve` was coherent on every branch —
//! so the consequence was latent rather than live, but the fields were all
//! `pub` and `Correlary` derived `Deserialize`, and the failure mode was the
//! bad direction: a correlary carrying a reference model *and* an `Unpriced`
//! basis charges a full shadow price while the dashboard prints "contributes
//! nothing to the savings figure" from the same record — silently inflating
//! the number the product is judged by.
//!
//! Both are now tagged enums, so the contradictions are unrepresentable rather
//! than merely unconstructed. **The first two assertions of this file are that
//! certain code no longer compiles**, which no runtime test can express: they
//! are recorded here as prose beside the code that replaced them, and the
//! compiler enforces them on every build.
//!
//! What *is* still runtime-testable is the deserialization ingress, which is
//! where an incoherent value could arrive from outside the crate's control.

use roundhouse_core::event::{Accounting, Usage};
use roundhouse_core::metrics::{
    Correlary, MetricsConfig, ModelAccounting, PricedBasis, ReferenceModel, ServingMode,
    ShadowPricing, TokenShape,
};
use roundhouse_core::routing::ProviderPricing;
use std::collections::HashMap;

const HOSTED: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

fn reference() -> ReferenceModel {
    ReferenceModel {
        provider: "anthropic".into(),
        model: "claude".into(),
        pricing: HOSTED,
        quality_prior: 0.6,
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: 0,
        output_tokens: output,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    }
}

/// Was: `Correlary { reference: Some(..), basis: Unpriced { .. } }`.
///
/// That literal no longer names anything — `Correlary` has two variants and
/// neither has both a reference and an unpriced basis — so the case this test
/// used to demonstrate is now a compile error. What remains testable is that
/// the two arms behave the way their names promise.
#[test]
fn an_unpriced_correlary_carries_no_reference_and_no_price() {
    let unpriced = Correlary::Unpriced {
        local_model: "llama".into(),
        reason: "no capability-comparable hosted model".into(),
    };

    assert!(unpriced.reference().is_none());
    assert_eq!(
        unpriced.shadow_cost_usd(&usage(1_000_000, 100_000)),
        0.0,
        "the unpriced arm has no rate card to price against, so this is \
         structurally zero rather than a zero some branch remembered to return"
    );
}

/// Was: `Correlary { reference: None, basis: Declared { .. } }`.
///
/// Also a compile error now: `Correlary::Priced` takes a `ReferenceModel`, not
/// an `Option<ReferenceModel>`, so a declared correlary cannot be missing the
/// model it was declared against.
#[test]
fn a_priced_correlary_always_has_the_model_it_is_priced_against() {
    let priced = Correlary::Priced {
        local_model: "llama".into(),
        reference: reference(),
        basis: PricedBasis::Declared {
            note: "matched on our eval suite".into(),
        },
    };

    assert_eq!(priced.reference().map(|r| r.model.as_str()), Some("claude"));
    assert!(
        priced.shadow_cost_usd(&usage(1_000_000, 0)) > 0.0,
        "and it prices, because the reference is not optional"
    );
}

/// The ingress that types alone do not close: a value arriving from outside
/// the crate. `/v1/metrics` is a public document, and anything round-tripping
/// it back in gets validated rather than trusted.
#[test]
fn deserialization_rejects_a_contradictory_correlary() {
    let unpriced_but_referenced = serde_json::json!({
        "local_model": "llama",
        "reference": {
            "provider": "anthropic",
            "model": "claude",
            "pricing": {
                "input_per_mtok_usd": 3.0,
                "cached_input_per_mtok_usd": 0.3,
                "cache_write_per_mtok_usd": 3.75,
                "output_per_mtok_usd": 15.0
            },
            "quality_prior": 0.6
        },
        "basis": { "kind": "unpriced", "reason": "no comparable model" }
    });
    let error = serde_json::from_value::<Correlary>(unpriced_but_referenced)
        .expect_err("unpriced yet carrying a reference model must be refused");
    assert!(
        error
            .to_string()
            .contains("unpriced yet carries a reference"),
        "the error says which contradiction: {error}"
    );

    let priced_but_unreferenced = serde_json::json!({
        "local_model": "llama",
        "basis": { "kind": "declared", "note": "matched on our eval suite" }
    });
    let error = serde_json::from_value::<Correlary>(priced_but_unreferenced)
        .expect_err("declared yet naming no reference model must be refused");
    assert!(
        error.to_string().contains("names no reference model"),
        "{error}"
    );
}

/// The wire contract did not move, so consumers built against the previous
/// shape keep working. The dashboard reads `basis.kind` and `reference`.
#[test]
fn the_serialized_shape_is_unchanged() {
    let priced = Correlary::Priced {
        local_model: "llama".into(),
        reference: reference(),
        basis: PricedBasis::Inferred {
            shape_distance: 0.25,
            considered: 3,
        },
    };
    let json = serde_json::to_value(&priced).unwrap();
    assert_eq!(json["local_model"], "llama");
    assert_eq!(json["reference"]["model"], "claude");
    assert_eq!(json["basis"]["kind"], "inferred");
    assert_eq!(json["basis"]["considered"], 3);

    let unpriced = Correlary::Unpriced {
        local_model: "tiny".into(),
        reason: "nothing comparable".into(),
    };
    let json = serde_json::to_value(&unpriced).unwrap();
    assert_eq!(json["basis"]["kind"], "unpriced");
    assert!(
        json.get("reference").is_none(),
        "an unpriced correlary omits the field rather than nulling it"
    );

    // And both round-trip, so the validating ingress accepts what we emit.
    for correlary in [priced, unpriced] {
        let text = serde_json::to_string(&correlary).unwrap();
        assert_eq!(serde_json::from_str::<Correlary>(&text).unwrap(), correlary);
    }
}

/// Was: `ModelMetrics { mode: Local, billed_usd: 42.0, .. }`.
///
/// Now a compile error — the money lives inside `ModelAccounting`, whose
/// variant *is* the mode, so a local row has no `billed_usd` field to set and
/// a hosted row has no `correlary`. The accessors report the structural zero.
#[test]
fn a_model_row_cannot_contradict_its_own_serving_mode() {
    let local = ModelAccounting::Local {
        shadow_usd: 12.5,
        correlary: Correlary::Unpriced {
            local_model: "llama".into(),
            reason: "nothing comparable".into(),
        },
        seat_tokens: Default::default(),
    };
    let row = roundhouse_core::metrics::ModelMetrics {
        provider: "dynamo".into(),
        model: "llama".into(),
        calls: 3,
        tokens: Default::default(),
        coverage: Default::default(),
        accounting: local,
    };

    assert_eq!(row.mode(), ServingMode::Local);
    assert_eq!(row.billed_usd(), 0.0, "a local row bills nothing, by shape");
    assert_eq!(row.cache_savings_usd(), 0.0);
    assert_eq!(row.shadow_usd(), 12.5);
    assert!(row.correlary().is_some());

    let hosted = roundhouse_core::metrics::ModelMetrics {
        provider: "anthropic".into(),
        model: "claude".into(),
        calls: 1,
        tokens: Default::default(),
        coverage: Default::default(),
        accounting: ModelAccounting::Frontier {
            billed_usd: 4.0,
            billed_measured_usd: 3.0,
            billed_estimated_usd: 1.0,
            cache_savings_usd: 0.5,
            seat_tokens: Default::default(),
        },
    };
    assert_eq!(hosted.mode(), ServingMode::Frontier);
    assert_eq!(
        hosted.shadow_usd(),
        0.0,
        "a hosted row is billed, not shadowed"
    );
    assert!(
        hosted.correlary().is_none(),
        "and it has no stand-in, because it is the thing others stand in for"
    );

    // The flattened wire form still carries `mode` and the applicable money at
    // the top level, so the dashboard's `m.mode === "local"` branch is intact.
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json["mode"], "local");
    assert_eq!(json["shadow_usd"], 12.5);
    assert!(
        json.get("billed_usd").is_none(),
        "absent, not a misleading zero"
    );
}

/// Control: the one in-tree constructor is coherent on every branch. If this
/// failed, the tests above would be guarding a door nobody uses.
#[test]
fn shadow_pricing_resolve_never_produces_an_incoherent_correlary() {
    let pricer = ShadowPricing::new(vec![reference()]).declare(
        "declared-model",
        "anthropic",
        "claude",
        "stated",
    );
    let mut observed = HashMap::new();
    observed.insert(
        ("anthropic".to_string(), "claude".to_string()),
        TokenShape::from_rollup(&usage(10_000, 1_000), 10).unwrap(),
    );

    let cases = [
        (
            "declared-model",
            0.6,
            Some(TokenShape::from_rollup(&usage(10_000, 1_000), 10).unwrap()),
        ),
        (
            "inferred-model",
            0.6,
            TokenShape::from_rollup(&usage(10_000, 1_000), 10),
        ),
        ("no-shape", 0.6, None),
        (
            "wrong-capability",
            0.05,
            TokenShape::from_rollup(&usage(10_000, 1_000), 10),
        ),
        ("bad-declaration", 0.6, None),
    ];
    for (model, quality, shape) in cases {
        let correlary = pricer.resolve(model, quality, shape, &observed, None);
        // Coherence is now a type-level property, so the check that remains is
        // that the value agrees with itself when priced.
        let priced = correlary.shadow_cost_usd(&usage(1_000_000, 0));
        match (&correlary, priced > 0.0) {
            (Correlary::Priced { .. }, true) | (Correlary::Unpriced { .. }, false) => {}
            (correlary, priced_nonzero) => panic!(
                "{model}: {correlary:?} priced-nonzero={priced_nonzero} disagrees with its own arm"
            ),
        }
    }

    // And a MetricsConfig built the ordinary way resolves the same values.
    let config = MetricsConfig::new(ShadowPricing::new(vec![reference()]).declare(
        "llama",
        "anthropic",
        "claude",
        "",
    ));
    assert!(matches!(
        config
            .pricing
            .resolve("llama", 0.6, None, &HashMap::new(), None),
        Correlary::Priced { .. }
    ));
}
