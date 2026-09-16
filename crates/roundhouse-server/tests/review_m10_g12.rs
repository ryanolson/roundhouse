// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M10 review finding G12: the attribution `import-benchmarks` refuses to emit
//! without has to be able to reach the surface that republishes the number.
//!
//! The finding was ruled valid with its **mechanism corrected**. Its proposed
//! remedy — an `attribution` field on each catalog-fragment entry — is the one
//! thing that must not happen: a catalog entry is `deny_unknown_fields`, so a
//! fragment carrying attribution would either fail to load or push a schema
//! this project invented for somebody else's data onto every catalog. The
//! provenance file is the attribution carrier, and what was missing is the
//! other end of the path: nothing in a shipped binary ever read that file, so
//! the dashboard published a routing saving priced through a capability gate
//! fed by third-party priors with no attribution reachable from any served
//! surface.
//!
//! What these tests pin is that end. The catalog loader looks for
//! `quality-prior.provenance.json` beside the file `ROUNDHOUSE_CATALOG` names,
//! the citation rides into `MetricsConfig`, and the metrics document publishes
//! it beside the figure it attributes. Discovery is never fatal: no file, a
//! malformed file, or an unattributable one leaves the deployment serving with
//! no citation rather than refusing to start.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use roundhouse_core::control::{Principal, PrincipalKey};
use roundhouse_core::metrics::{MetricsFold, MetricsSnapshot, Scope};
use roundhouse_core::now_ms;
use roundhouse_server::catalog_config::CatalogConfig;

/// One priced hosted model — the smallest catalog that loads.
fn catalog_json() -> String {
    json!({
        "models": [{
            "provider": "openrouter",
            "model": "anthropic/claude-sonnet-4",
            "wire_protocol": "openai_responses",
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
        }],
        "providers": {
            "openrouter": {
                "base_url": "https://openrouter.test/api/v1",
                "routes": { "responses": "/responses" },
                "auth": { "env": "OPENROUTER_API_KEY" }
            }
        }
    })
    .to_string()
}

/// A directory of this test's own, under cargo's per-target temp dir.
///
/// `CARGO_TARGET_TMPDIR` rather than `/tmp`: it is cleaned with the build
/// directory, it is not shared with anything else on the machine, and a test
/// that wrote a file named after a real deployment's convention into a shared
/// temp directory is a test that could be read by another process looking for
/// exactly that name.
fn case_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("g12-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("a writable scratch directory");
    dir
}

/// Write a catalog and, optionally, a provenance file beside it; load it.
fn loaded(name: &str, provenance: Option<&str>) -> CatalogConfig {
    let dir = case_dir(name);
    let catalog = dir.join("catalog.json");
    fs::write(&catalog, catalog_json()).expect("write catalog");
    if let Some(provenance) = provenance {
        fs::write(dir.join("quality-prior.provenance.json"), provenance).expect("write provenance");
    }
    CatalogConfig::load(&catalog).expect("the catalog itself is valid in every case here")
}

/// What the served document says, which is the only thing an operator sees.
fn published(config: &CatalogConfig) -> Value {
    published_in(config, Scope::Deployment)
}

fn published_in(config: &CatalogConfig, scope: Scope<'_>) -> Value {
    let snapshot = MetricsSnapshot::build(
        &MetricsFold::default(),
        scope,
        &config.metrics_config(),
        now_ms(),
    );
    serde_json::to_value(snapshot).expect("the metrics document serializes")
}

/// A single-source import: `meta.citation` is a sentence, and it is the line.
#[test]
fn a_provenance_file_beside_the_catalog_is_published_with_the_savings_figure() {
    let provenance = json!({
        "endpoint": "https://openrouter.ai/api/v1/benchmarks",
        "meta": {
            "version": "v1",
            "as_of": "2026-06-03T12:00:00Z",
            "citation": "Artificial Analysis Intelligence Index v3",
            "source": "artificial-analysis",
        },
        "entries": [{
            "provider": "openrouter",
            "model": "anthropic/claude-sonnet-4",
            "quality_prior": 0.62,
            "attribution": { "citation": "Artificial Analysis Intelligence Index v3" },
        }],
    })
    .to_string();

    let config = loaded("single-source", Some(&provenance));
    let citation = config
        .quality_prior_citation
        .clone()
        .expect("a provenance file beside the catalog is the whole trigger");
    assert!(
        citation.contains("Artificial Analysis Intelligence Index v3"),
        "the upstream's own citation, verbatim: {citation}"
    );
    // Stamped with the dataset version and date. An unversioned credit reads as
    // current the day the upstream leaderboard moves, which is exactly what
    // CLAUDE.md asks an imported index to be pinned against.
    assert!(citation.contains("v1"), "{citation}");
    assert!(citation.contains("2026-06-03T12:00:00Z"), "{citation}");

    let document = published(&config);
    assert_eq!(
        document["quality_prior_citation"], citation,
        "the served document is where the obligation is discharged -- a citation \
         read at boot and kept in the process is a citation nobody is credited \
         by: {document}"
    );
    // The field name the dashboard's own JS reads, spelled out: this document
    // has no container-level rename, and a camelCased wire name would leave the
    // citation element permanently hidden with every assertion above still
    // green.
    assert!(
        document.get("quality_prior_citation").is_some(),
        "the wire name is snake_case, which is what dashboard.html indexes: {document}"
    );

    // And under the *scoped* read a turn key gets, not only the deployment
    // document an admin gets. `MetricsSnapshot::build` scopes every figure; the
    // citation is not a figure, and an attribution that vanished for the reader
    // who happens to hold a turn key would be an obligation discharged for some
    // readers only.
    let principal = Principal::new("acme", "ada");
    let scoped = published_in(&config, Scope::Principal(&PrincipalKey::from(&principal)));
    assert_eq!(
        scoped["quality_prior_citation"], citation,
        "the citation survives the principal-scoped path: {scoped}"
    );
}

/// The multi-source case, which is the ordinary one: `meta.citation` is null
/// and the schema says to attribute each item by its own `source`.
#[test]
fn a_multi_source_import_is_attributed_by_the_publishers_its_entries_name() {
    let provenance = json!({
        "meta": { "version": "v1", "as_of": "2026-06-03T12:00:00Z", "citation": null },
        "entries": [
            { "attribution": { "citation": null, "source": "artificial-analysis" } },
            { "attribution": { "citation": null, "source": "design-arena" } },
            // A repeat, to prove the line names each publisher once.
            { "attribution": { "citation": null, "source": "artificial-analysis" } },
        ],
    })
    .to_string();

    let citation = loaded("multi-source", Some(&provenance))
        .quality_prior_citation
        .expect("a null meta.citation is the documented multi-source case, not a failure");
    assert!(citation.contains("artificial-analysis"), "{citation}");
    assert!(citation.contains("design-arena"), "{citation}");
    assert_eq!(
        citation.matches("artificial-analysis").count(),
        1,
        "each publisher credited once: {citation}"
    );
}

/// No provenance file: no line, and nothing else changes.
///
/// The control that makes the tests above non-tautological — and the shipped
/// posture, since a deployment whose priors are its own configuration has
/// nothing to attribute and must not be given a blank credit line to read.
#[test]
fn a_catalog_with_no_provenance_beside_it_publishes_no_citation() {
    let config = loaded("no-file", None);
    assert!(config.quality_prior_citation.is_none());
    let document = published(&config);
    assert!(
        document["quality_prior_citation"].is_null(),
        "absent is `null`, never an empty string a dashboard would render as a \
         credit that failed to load: {document}"
    );
}

/// A malformed provenance file is a missing citation, never a refused boot.
///
/// `CatalogConfig::load` is load-or-die because an operator named that path.
/// Nothing named this one — it is discovered — so a stray or half-written file
/// in the catalog's directory must not be able to stop a deployment starting.
#[test]
fn a_malformed_provenance_file_does_not_stop_the_catalog_loading() {
    let config = loaded("malformed", Some("{ this is not json"));
    assert!(
        config.quality_prior_citation.is_none(),
        "nothing to cite, and the catalog still loaded"
    );
    assert_eq!(
        config.models.len(),
        1,
        "the catalog itself is unaffected by a file it does not name"
    );
}

/// A provenance file that attributes nothing is treated as no attribution.
///
/// Rather than a citation line reading "attributed per source:" with nothing
/// after it, which credits nobody while looking like it did.
#[test]
fn a_provenance_file_that_attributes_nothing_publishes_no_citation() {
    let provenance = json!({
        "meta": { "version": "v1", "as_of": "2026-06-03T12:00:00Z", "citation": null },
        "entries": [{ "attribution": { "citation": null, "source": "" } }],
    })
    .to_string();
    assert!(
        loaded("unattributable", Some(&provenance))
            .quality_prior_citation
            .is_none()
    );
}
