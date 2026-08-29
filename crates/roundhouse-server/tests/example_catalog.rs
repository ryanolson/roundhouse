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

/// **The example's second OpenRouter definition authenticates the way that
/// route actually requires** (F4, M11.0 thermo-nuclear review).
///
/// The file is where the spelling of a stored key is now stated, so the file is
/// where it can now be stated wrong — and wrong here is a 401 on every turn,
/// which an operator reads as a bad key rather than as a bad line. That is
/// exactly the class of mistake this example exists to not teach.
///
/// The `anthropic` definition beside it is the control: it must stay on the
/// default. If both providers ended up on one spelling, whichever one it was,
/// the other would be unreachable — which is the finding this pins the fix of.
#[test]
fn the_examples_two_messages_providers_spell_their_keys_differently() {
    use roundhouse_fleet::WireProtocol;
    use roundhouse_fleet::anthropic_messages::StoredAuthStyle;

    let config = CatalogConfig::load(example_path()).unwrap();
    let openrouter = config
        .providers
        .get("openrouter-messages")
        .expect("the example demonstrates the `anthropic_messages` dialect's second provider");
    assert_eq!(
        openrouter.auth.stored_auth_style(),
        Some(StoredAuthStyle::Bearer),
        "OpenRouter's /messages route answers an `x-api-key` with \"Missing \
         Authentication header\" on every attempt"
    );
    // The style is only reachable by a client that has a route to send, so the
    // two halves are asserted together: a definition carrying one without the
    // other teaches a shape that cannot dispatch.
    assert_eq!(
        openrouter
            .routes
            .for_dialect(WireProtocol::AnthropicMessages),
        Some("/messages")
    );

    let anthropic = config
        .providers
        .get("anthropic")
        .expect("the example's first-party Messages provider");
    assert_eq!(
        anthropic.auth.stored_auth_style(),
        Some(StoredAuthStyle::XApiKey),
        "api.anthropic.com authenticates a bare `x-api-key` and answers a bearer \
         with a 401 whose message does not say why"
    );
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
///
/// **All four rates, not just input (M11.0).** The file gained an
/// `anthropic_messages` entry, and that is the first dialect on which
/// `cache_write_per_mtok_usd` is a distinct published price rather than an unused
/// field — so it is the first entry where a "helpfully" filled-in rate card could
/// rot in a column this test was not looking at. Checking one of four was enough
/// while three of them were structurally zero everywhere; it is not any more.
#[test]
fn the_example_prices_are_placeholders_not_a_rate_card() {
    let config = CatalogConfig::load(example_path()).unwrap();
    for spec in &config.models {
        for (field, rate) in [
            ("input_per_mtok_usd", spec.pricing.input_per_mtok_usd),
            (
                "cached_input_per_mtok_usd",
                spec.pricing.cached_input_per_mtok_usd,
            ),
            (
                "cache_write_per_mtok_usd",
                spec.pricing.cache_write_per_mtok_usd,
            ),
            ("output_per_mtok_usd", spec.pricing.output_per_mtok_usd),
        ] {
            assert_eq!(
                rate, 0.0,
                "`{}/{}` carries a non-zero `{field}`; the example must not ship a rate card",
                spec.provider, spec.model
            );
        }
        let is_a_real_pinned_id_shown_for_illustration = spec.model.contains('/');
        assert!(
            spec.model.contains("REPLACE") || is_a_real_pinned_id_shown_for_illustration,
            "`{}` reads like a real model name; the example must be obviously a template \
             or, if it is a real id shown for illustration, spelled as `org/model`",
            spec.model
        );
    }
}
