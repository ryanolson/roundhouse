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
#[test]
fn the_example_prices_are_placeholders_not_a_rate_card() {
    let config = CatalogConfig::load(example_path()).unwrap();
    for spec in &config.models {
        assert_eq!(
            spec.pricing.input_per_mtok_usd, 0.0,
            "`{}/{}` carries a non-zero price; the example must not ship a rate card",
            spec.provider, spec.model
        );
        assert!(
            spec.model.contains("REPLACE"),
            "`{}` reads like a real model name; the example must be obviously a template",
            spec.model
        );
    }
}
