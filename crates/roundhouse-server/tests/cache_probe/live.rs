// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The live run. Spends real money; see `Cargo.toml`'s `e2e-frontier`.
//!
//! Three settings, all the operator's, none defaulted:
//!
//! | Variable | What |
//! |---|---|
//! | `ROUNDHOUSE_PROBE_CATALOG` | path to a catalog JSON with the model and its real rate card |
//! | `ROUNDHOUSE_PROBE_MODEL` | the pinned `provider/model` to probe |
//! | `ROUNDHOUSE_PROBE_LIMIT_USD` | the project ceiling this run may not exceed |
//!
//! **Which variable holds the key is not among them.** The catalog's provider
//! definition already says — `auth.env` is how every deployment names it — so a
//! probe setting beside it would be a second place to get the same answer
//! wrong, and the two could disagree. The value itself is injected by `openv`
//! and is never written into a command line, a fixture or this file; nothing
//! here prints it.

use std::sync::Arc;

use roundhouse_core::routing::CacheModel;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::StaticFrontierCatalog;
use roundhouse_server::CatalogConfig;
use roundhouse_server::catalog_config::ProviderConfig;

use crate::engine::{admission, engine};
use crate::preflight::probe_inputs;
use crate::probe::two_turn_probe;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "{name} is required: this suite spends real money, so it refuses to \
             guess a catalog, a model, a ceiling or where the key lives"
        )
    })
}

/// Everything the run needs, or a refusal naming what is missing.
///
/// Every check runs before the first request, because each one of them is
/// a way to spend money on a run that could not have answered the
/// question.
fn preflight() -> (StaticFrontierCatalog, String, ProviderConfig, String, f64) {
    let path = required("ROUNDHOUSE_PROBE_CATALOG");
    let pinned = required("ROUNDHOUSE_PROBE_MODEL");
    let limit_usd: f64 = required("ROUNDHOUSE_PROBE_LIMIT_USD")
        .parse()
        .expect("ROUNDHOUSE_PROBE_LIMIT_USD must be a dollar figure");

    let config = CatalogConfig::load(&path).expect("the catalog must load and validate");
    let (provider, model) = pinned
        .split_once('/')
        .expect("ROUNDHOUSE_PROBE_MODEL is `provider/model`");
    let spec = config
        .models
        .iter()
        .find(|spec| spec.provider == provider && spec.model == model)
        .unwrap_or_else(|| panic!("{pinned} is not in {path}"))
        .clone();
    // The dialect, the ceiling and all four prices, through the same
    // function the offline suite exercises with explicit inputs.
    probe_inputs(limit_usd, &spec).unwrap_or_else(|refusal| panic!("{refusal}"));
    // The cache model the *router* prices this run against. It does not
    // decide whether a marker is placed — `body` places one whenever there
    // is a stable prefix, whatever the model says — so this is about the
    // prediction the probe is checking, not about the wire.
    assert!(
        matches!(spec.cache_model, CacheModel::Deterministic { .. }),
        "{pinned} is priced on a cache this deployment does not model as \
         deterministic, so the read this probe looks for is not the one the \
         router expects"
    );

    // The whole definition, not just its base URL: the transport reads its
    // route, its auth spelling and its static headers, exactly as `main`
    // does.
    let definition = config
        .providers
        .get(provider)
        .unwrap_or_else(|| panic!("the catalog defines no provider `{provider}`"))
        .clone();
    // The variable the *catalog* names, which is the one a deployment
    // already sets for this provider.
    let key_var = &definition.auth.env;
    let secret = std::env::var(key_var).unwrap_or_else(|_| {
        panic!(
            "the catalog's `{provider}` definition says its key lives in {key_var}, \
             and that variable is unset — inject it with `openv`"
        )
    });
    assert!(!secret.trim().is_empty(), "{key_var} is set but empty");

    (
        StaticFrontierCatalog::new(vec![spec]),
        provider.to_string(),
        definition,
        secret,
        limit_usd,
    )
}

/// Two real turns, and the counters they reported.
#[tokio::test]
async fn a_real_two_turn_session_reports_both_cache_counters() {
    let (catalog, provider, definition, secret, limit_usd) = preflight();
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&definition, &provider, catalog, Arc::clone(&store));

    let report = two_turn_probe(&engine, &store, &admission(limit_usd, &provider, &secret))
        .await
        .expect("the two turns are served");

    // Printed rather than asserted, because the answer is the evidence:
    // a negative result is the more important one and must not read as a
    // broken test. Run with `--nocapture` and record the line in the
    // ruling.
    println!("{}", report.render());
    assert!(
        report.first.reported && report.second.reported,
        "the provider sent no usage for at least one turn, so this run's \
         zeroes are roundhouse's own estimate and not an observation of \
         its cache:\n{}",
        report.render()
    );
    assert!(
        report.first.input > 0,
        "the provider reported no input tokens at all, so nothing here is \
         evidence about its cache"
    );
}
