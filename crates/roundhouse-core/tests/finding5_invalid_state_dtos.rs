// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Validation of review finding F5: "invalid-state DTOs".
//!
//! The claim under test is narrow and mechanical: `Correlary` encodes its
//! invariant as `Option<ReferenceModel>` beside a separate `CorrelaryBasis`
//! tag, and its own doc comment states the invariant — "`None` exactly when
//! the basis is [`CorrelaryBasis::Unpriced`]". Nothing enforces it. Both
//! fields are `pub` and the type is `Deserialize`, so a contradictory value is
//! constructible by any consumer of the public API and by any JSON that
//! reaches `serde`.
//!
//! These tests assert the invariant the doc comment claims. They fail where it
//! is not actually enforced.

use std::collections::HashMap;

use roundhouse_core::event::Usage;
use roundhouse_core::metrics::{
    Correlary, CorrelaryBasis, ReferenceModel, ShadowPricing, TokenShape,
};
use roundhouse_core::routing::ProviderPricing;

fn expensive_rate_card() -> ProviderPricing {
    ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    }
}

fn reference() -> ReferenceModel {
    ReferenceModel {
        provider: "anthropic".into(),
        model: "claude-sonnet".into(),
        pricing: expensive_rate_card(),
        quality_prior: 0.62,
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 1_000_000,
        cached_input_tokens: 0,
        output_tokens: 1_000_000,
        reasoning_tokens: 0,
        ..Default::default()
    }
}

/// The invariant `Correlary`'s doc comment states, written as a predicate.
///
/// A correlary is coherent when the presence of a reference model agrees with
/// what the basis says was decided: `Unpriced` means no stand-in was found and
/// therefore no price can be charged; `Declared` and `Inferred` both name a
/// stand-in and therefore must carry one.
fn is_coherent(correlary: &Correlary) -> bool {
    match (&correlary.reference, &correlary.basis) {
        (None, CorrelaryBasis::Unpriced { .. }) => true,
        (Some(_), CorrelaryBasis::Declared { .. } | CorrelaryBasis::Inferred { .. }) => true,
        _ => false,
    }
}

/// (b) reference: Some + basis: Unpriced.
///
/// The basis says no stand-in could be justified and no shadow price was
/// charged. `shadow_cost_usd` charges one anyway, because it reads the
/// `Option` and never looks at the basis.
#[test]
#[ignore = "F5: validated defect, unfixed — Correlary and ModelMetrics accept states their own docs call impossible"]
fn an_unpriced_correlary_cannot_also_carry_a_reference_model() {
    let contradictory = Correlary {
        local_model: "llama-70b".into(),
        reference: Some(reference()),
        basis: CorrelaryBasis::Unpriced {
            reason: "no hosted model within 0.10 of quality prior 0.50".into(),
        },
    };

    let charged = contradictory.shadow_cost_usd(&usage());
    let json = serde_json::to_value(&contradictory).unwrap();

    assert!(
        is_coherent(&contradictory),
        "Correlary's own doc comment says `reference` is `None` *exactly when* the basis \
         is Unpriced, but the public API accepts the contradiction and prices it.\n\
         \n\
         shadow_cost_usd  = ${charged:.2}   (expected $0.00 for an unpriced basis)\n\
         serialized JSON  = {json}\n\
         \n\
         The dashboard reads exactly this document. dashboard.html renders the basis chip \
         from `basis.kind` and the dollar column from `c.reference ? usd(m.shadow_usd) : dash`, \
         so this row reads \"Not priced\" beside a reference model and a positive dollar \
         figure; and the note driven by `basis.kind === \"unpriced\"` tells the reader this \
         model's traffic \"contributes nothing to the savings figure\" while its \
         shadow_usd is summed into savings.routing_savings_usd."
    );
}

/// (b), the other direction: reference: None + basis: Declared.
///
/// A human stated an equivalence; the type lets the reference go missing, and
/// the shadow price silently collapses to zero while the dashboard still shows
/// the "Declared" chip and the operator's note.
#[test]
#[ignore = "F5: validated defect, unfixed — Correlary and ModelMetrics accept states their own docs call impossible"]
fn a_declared_correlary_cannot_be_missing_its_reference_model() {
    let contradictory = Correlary {
        local_model: "llama-70b".into(),
        reference: None,
        basis: CorrelaryBasis::Declared {
            note: "within 2 points on our internal eval".into(),
        },
    };

    let charged = contradictory.shadow_cost_usd(&usage());

    assert!(
        is_coherent(&contradictory),
        "a Declared basis with no reference is constructible through the public API. \
         The declaration is still rendered to the reader (chip + note), but \
         shadow_cost_usd = ${charged:.2}, so the declared equivalence contributes nothing \
         and nothing anywhere says so."
    );
}

/// The same contradiction arriving through `serde`, not through a struct
/// literal. `Correlary` derives `Deserialize`, so this is an untrusted-input
/// ingress and not only an API-shape complaint.
#[test]
#[ignore = "F5: validated defect, unfixed — Correlary and ModelMetrics accept states their own docs call impossible"]
fn deserialization_rejects_a_contradictory_correlary() {
    let json = r#"{
      "local_model": "llama-70b",
      "reference": {
        "provider": "anthropic",
        "model": "claude-sonnet",
        "pricing": {
          "input_per_mtok_usd": 3.0,
          "cached_input_per_mtok_usd": 0.3,
          "cache_write_per_mtok_usd": 3.75,
          "output_per_mtok_usd": 15.0
        },
        "quality_prior": 0.62
      },
      "basis": { "kind": "unpriced", "reason": "no comparable hosted model" }
    }"#;

    let parsed: Result<Correlary, _> = serde_json::from_str(json);

    match parsed {
        Err(_) => {} // the invariant is enforced at the deserialization boundary
        Ok(correlary) => panic!(
            "serde accepted a correlary that is unpriced and priced at once: \
             basis = {:?}, reference = {:?}, shadow_cost_usd = ${:.2}",
            correlary.basis,
            correlary.reference.as_ref().map(|r| &r.model),
            correlary.shadow_cost_usd(&usage()),
        ),
    }
}

/// Control: the crate's own constructor never produces an incoherent value.
///
/// This is what separates "unenforced invariant" from "live miscomputation".
/// `ShadowPricing::resolve` is the only in-tree path that builds a `Correlary`,
/// and every branch of it is coherent — including the one the module documents
/// as the trap, a declaration naming a model with no rate card.
#[test]
fn shadow_pricing_resolve_never_produces_an_incoherent_correlary() {
    let shape = TokenShape::from_rollup(&usage(), 10);
    let mut observed = HashMap::new();
    observed.insert(
        ("anthropic".to_string(), "claude-sonnet".to_string()),
        TokenShape::from_rollup(&usage(), 10).unwrap(),
    );

    let pricing = ShadowPricing::new(vec![reference()]);
    let cases = vec![
        // inferred: gate passes, hosted model has observed traffic
        pricing.resolve("llama-70b", 0.62, shape, &observed),
        // unpriced: nothing within the capability band
        pricing.resolve("tiny", 0.01, shape, &observed),
        // unpriced: no local traffic to compare
        pricing.resolve("llama-70b", 0.62, None, &observed),
        // unpriced: no comparable hosted model has been called
        pricing.resolve("llama-70b", 0.62, shape, &HashMap::new()),
        // declared, resolvable
        ShadowPricing::new(vec![reference()])
            .declare("llama-70b", "anthropic", "claude-sonnet", "eval parity")
            .resolve("llama-70b", 0.62, shape, &observed),
        // declared against a model with no rate card
        ShadowPricing::new(vec![reference()])
            .declare("llama-70b", "openai", "gpt-nonexistent", "eval parity")
            .resolve("llama-70b", 0.62, shape, &observed),
    ];

    for correlary in &cases {
        assert!(
            is_coherent(correlary),
            "ShadowPricing::resolve produced an incoherent correlary: {correlary:?}"
        );
    }
}

/// (c) ModelMetrics: `mode` plus mutually exclusive money fields plus an
/// optional comparison, with nothing tying them together.
///
/// The doc comments state three invariants: `billed_usd` is "always zero for
/// [`ServingMode::Local`]", `shadow_usd` is "zero for hosted models", and
/// `correlary` is "present only for local models". All three are constructible
/// false through the public API.
///
/// Weaker than the `Correlary` case on purpose, and the report says so: no
/// library function *consumes* a `ModelMetrics` built outside the crate, so
/// this is a DTO-shape fact rather than a reachable miscomputation.
#[test]
#[ignore = "F5: validated defect, unfixed — Correlary and ModelMetrics accept states their own docs call impossible"]
fn model_metrics_cannot_contradict_its_own_serving_mode() {
    use roundhouse_core::metrics::{Coverage, ModelMetrics, ServingMode, TokenBreakdown};

    let incoherent = vec![
        // local, yet billed money and carrying no correlary
        ModelMetrics {
            mode: ServingMode::Local,
            provider: "roundhouse".into(),
            model: "llama-70b".into(),
            calls: 4,
            tokens: TokenBreakdown::default(),
            coverage: Coverage::default(),
            billed_usd: 42.0,
            shadow_usd: 0.0,
            cache_savings_usd: 0.0,
            correlary: None,
        },
        // hosted, yet shadow-priced against a correlary it cannot have
        ModelMetrics {
            mode: ServingMode::Frontier,
            provider: "anthropic".into(),
            model: "claude-sonnet".into(),
            calls: 4,
            tokens: TokenBreakdown::default(),
            coverage: Coverage::default(),
            billed_usd: 10.0,
            shadow_usd: 7.0,
            cache_savings_usd: 0.0,
            correlary: Some(Correlary {
                local_model: "llama-70b".into(),
                reference: Some(reference()),
                basis: CorrelaryBasis::Declared { note: "n/a".into() },
            }),
        },
    ];

    for row in &incoherent {
        let coherent = match row.mode {
            ServingMode::Local => row.billed_usd == 0.0 && row.correlary.is_some(),
            ServingMode::Frontier => row.shadow_usd == 0.0 && row.correlary.is_none(),
        };
        assert!(
            coherent,
            "ModelMetrics accepts a row contradicting its own documented invariants: \
             mode={:?} billed_usd={} shadow_usd={} correlary={}\n\
             serialized: {}",
            row.mode,
            row.billed_usd,
            row.shadow_usd,
            if row.correlary.is_some() { "Some" } else { "None" },
            serde_json::to_string(row).unwrap(),
        );
    }
}
