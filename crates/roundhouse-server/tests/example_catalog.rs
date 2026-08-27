// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The shipped example catalog is a real catalog.
//!
//! `examples/catalog.example.json` is the file an operator copies to start
//! from, and the config boundary now rejects several shapes it did not before —
//! duplicate identities, negative rates, off-scale priors, correlaries naming a
//! model the catalog does not price. An example that fails its own validator is
//! worse than no example: it teaches the format wrong and the failure surfaces
//! on someone else's deployment.

use roundhouse_server::CatalogConfig;

/// The example's own declared correlary actually prices a local turn.
///
/// **Validation is not the same question, which is how this gap survived.**
/// `the_shipped_example_catalog_parses_and_validates` asks whether the
/// correlary's target is in `models`; whether the *numbers beside it* produce a
/// priced counterfactual is asked by the capability gate, and the fixture that
/// exercises the gate is a hand-built one whose priors were chosen to pass it.
/// The shipped file pairs a local model declared at 0.62 with a reference at
/// 0.90 under a `capability_band` of 0.10 — a gap wider than the band — so an
/// example whose savings story quietly resolved `Unpriced` would look exactly
/// like an example that worked, on a dashboard that reported nothing.
///
/// Declared correlaries are the operator's own statement and are not gated
/// today, which is why this passes; the basis is deliberately *not* pinned, so
/// a later ruling that sends declared pairs through the gate turns this red on
/// the example's numbers rather than on a deployment's.
#[test]
fn the_examples_declared_correlary_prices_a_local_turn() {
    let config = CatalogConfig::load(example_path()).unwrap();
    let metrics = config.metrics_config();
    // Or the loop below is a test that passes by having nothing to check: the
    // declared-correlary story is one of the two things this file exists to
    // demonstrate, and an edit that dropped it would otherwise ship green.
    assert!(
        !config.correlaries.is_empty(),
        "the example must keep demonstrating a declared correlary"
    );
    for correlary in &config.correlaries {
        let prior = config
            .local_quality
            .get(&correlary.local_model)
            .copied()
            .unwrap_or(config.default_local_quality);
        let resolved = metrics.pricing.resolve(
            &correlary.local_model,
            prior,
            None,
            &std::collections::HashMap::new(),
            None,
        );
        // `Priced`, not merely "names a reference": an `Unpriced` correlary can
        // still carry the model it was refused against, so asking for a
        // reference would pass on exactly the deployment this is about — one
        // whose savings column is empty and whose file looks fine.
        assert!(
            matches!(resolved, roundhouse_core::metrics::Correlary::Priced { .. }),
            "the example declares `{}` stands in for `{}/{}`, and the dashboard prices \
             that at nothing: {resolved:?}",
            correlary.local_model,
            correlary.provider,
            correlary.model,
        );
    }
}

fn example_path() -> std::path::PathBuf {
    // From the crate root up to the workspace root.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/catalog.example.json")
}

#[test]
fn the_shipped_example_catalog_parses_and_validates() {
    let path = example_path();
    let config = CatalogConfig::load(&path)
        .unwrap_or_else(|error| panic!("the shipped example must validate: {error}"));

    assert!(!config.models.is_empty());
    // Every declared correlary resolves, which is one of the things the
    // boundary now checks and the easiest to get wrong when editing by hand.
    for correlary in &config.correlaries {
        assert!(
            config
                .models
                .iter()
                .any(|m| m.provider == correlary.provider && m.model == correlary.model),
            "correlary for `{}` names `{}/{}`, absent from the example's own models",
            correlary.local_model,
            correlary.provider,
            correlary.model,
        );
    }
}

/// The example ships placeholder prices of zero, deliberately: a real rate card
/// in source goes stale, and zeros make it obvious the file has not been filled
/// in. This pins that they are zero rather than plausible-looking, so nobody
/// later "helpfully" replaces them with real numbers that then rot.
///
/// Two kinds of entry ship here, and only one needs a `REPLACE` name. The
/// `openai` entry is a template an operator overwrites with the model they
/// actually call. The `openrouter` entries are the opposite on
/// purpose — P3 wants a *real*, fully-qualified, dated id shown (the whole
/// point being "write the full id, a bare one is not what you think it is"),
/// and a `REPLACE` name there would defeat that. Both kinds still have to
/// carry a zero price, which is the half of this test that actually stops a
/// rate card from rotting in source; the name check below only has to
/// distinguish "a placeholder" from "a real id shown for illustration", and a
/// full id's `provider/model` slash is what tells the two apart without
/// hard-coding a provider name here.
#[test]
fn the_example_prices_are_placeholders_not_a_rate_card() {
    let config = CatalogConfig::load(example_path()).unwrap();
    for spec in &config.models {
        assert_eq!(
            spec.pricing.input_per_mtok_usd, 0.0,
            "`{}/{}` carries a non-zero price; the example must not ship a rate card",
            spec.provider, spec.model
        );
        let is_a_real_pinned_id_shown_for_illustration = spec.model.contains('/');
        assert!(
            spec.model.contains("REPLACE") || is_a_real_pinned_id_shown_for_illustration,
            "`{}` reads like a real model name; the example must be obviously a template \
             or, if it is a real id shown for illustration, spelled as `org/model`",
            spec.model
        );
    }
}
