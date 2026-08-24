// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Deployment configuration for the catalog, the rate card, and the correlaries.
//!
//! One file, because these three are one fact seen from three angles. The
//! catalog is what the router may choose between; the rate card is what those
//! choices cost; the correlaries are what our own models stand in for when
//! they are priced. Splitting them across separate configuration would let the
//! price the router optimizes against drift from the price the dashboard
//! reports saving, and those two numbers disagreeing is worse than either being
//! wrong — it is unfalsifiable.
//!
//! Prices are not in source, here or anywhere: rate cards change, and a
//! constant in a binary goes stale silently. `roundhouse-fleet`'s
//! `frontier` module states that rule; this is the mechanism that lets a
//! deployment honor it.
//!
//! The config format *is* [`FrontierModelSpec`], deserialized. Adding a field
//! to the spec therefore changes the format by construction, rather than
//! leaving a hand-written schema to fall behind it.

pub mod providers;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use roundhouse_core::metrics::{DEFAULT_CAPABILITY_BAND, MetricsConfig};
use roundhouse_fleet::{FrontierModelSpec, StaticFrontierCatalog};

pub use providers::{BUILT_IN_OPENAI, ProviderAuth, ProviderConfig, ProviderRoutes};

/// Path to a catalog JSON file. Absent means the built-in offline stub.
pub const CATALOG_VAR: &str = "ROUNDHOUSE_CATALOG";

/// A stated equivalence between one of our models and a hosted one.
#[derive(Debug, Clone, Deserialize)]
pub struct CorrelaryConfig {
    /// The local model's name, as `EngineConfig::local_model` reports it.
    pub local_model: String,
    pub provider: String,
    pub model: String,
    /// Why. Shown verbatim on the dashboard, because a reader deciding whether
    /// to trust the savings figure is really deciding whether to trust this.
    #[serde(default)]
    pub note: String,
}

/// What a deployment supplies.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogConfig {
    /// Hosted models the router may choose between, with their prices.
    pub models: Vec<FrontierModelSpec>,
    /// Where each [`FrontierModelSpec::provider`] actually is, keyed by that
    /// name.
    ///
    /// Absent — the shape of every catalog written before M10.1 — means every
    /// entry names [`BUILT_IN_OPENAI`], which is the implicit definition the
    /// `ROUNDHOUSE_OPENAI_API_BASE` wiring already supplied. See
    /// [`providers`]: this is the section that lets one process hold a client
    /// per origin instead of one client for the whole catalog.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Declared local-to-hosted equivalences. Anything not declared here is
    /// inferred from traffic shape, subject to the capability gate.
    #[serde(default)]
    pub correlaries: Vec<CorrelaryConfig>,
    /// Declared capability of each local model, 0.0..=1.0, keyed by model name.
    #[serde(default)]
    pub local_quality: HashMap<String, f64>,
    /// Used for a local model absent from `local_quality`.
    #[serde(default = "default_local_quality")]
    pub default_local_quality: f64,
    /// How far apart two models' quality priors may be and still be compared.
    #[serde(default = "default_capability_band")]
    pub capability_band: f64,
}

/// A value the capability gate compares must live on the scale the gate is
/// defined on. Outside it, a band silently widens to admit everything or closes
/// to admit nothing, and either way the gate stops being a gate.
fn unit_interval(
    path: &str,
    model: &str,
    field: &'static str,
    value: f64,
) -> Result<(), CatalogError> {
    if (0.0..=1.0).contains(&value) {
        return Ok(());
    }
    Err(CatalogError::InvalidValue {
        path: path.to_string(),
        model: model.to_string(),
        field,
        value,
        expected: "the capability scale is 0.0..=1.0",
    })
}

fn default_local_quality() -> f64 {
    0.5
}

fn default_capability_band() -> f64 {
    DEFAULT_CAPABILITY_BAND
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("could not read catalog `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse catalog `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("catalog `{path}` lists no models, so no turn could be routed anywhere")]
    Empty { path: String },
    #[error(
        "catalog `{path}` lists `{provider}/{model}` more than once. Two prices for one \
         model identity do not resolve the same way on both sides: the router seeds its \
         ledger by insertion and keeps the last, while the dashboard looks up a rate card \
         by search and finds the first, so the price a turn is chosen on and the price it \
         is reported at would differ silently"
    )]
    DuplicateModel {
        path: String,
        provider: String,
        model: String,
    },
    #[error("catalog `{path}`: `{model}` has {field} = {value}, but {expected}")]
    InvalidValue {
        path: String,
        model: String,
        field: &'static str,
        value: f64,
        expected: &'static str,
    },
    #[error(
        "catalog `{path}`: the correlary for `{local_model}` names `{provider}/{model}`, \
         which is not in this catalog, so that model's traffic would silently go unpriced"
    )]
    UnknownCorrelaryTarget {
        path: String,
        local_model: String,
        provider: String,
        model: String,
    },
    #[error(
        "catalog `{path}`: `{provider}/{model}` names a provider nothing defines. Add a \
         `\"providers\"` entry for `{provider}` naming its base URL, its routes and where its \
         key lives, or spell the entry `\"{BUILT_IN_OPENAI}\"`, which is the one definition \
         this build supplies implicitly. Refused here rather than at the turn that would \
         dispatch it: a provider with no definition has no client, and a router that picked \
         it would fail one tenant's turn for a mistake made in a file"
    )]
    UndefinedProvider {
        path: String,
        provider: String,
        model: String,
    },
    #[error(
        "catalog `{path}`: `{provider}/{model}` speaks `{dialect}`, but the `{provider}` \
         provider declares no `routes.{field}` to send it to. A dispatch would have nowhere \
         to POST, so the entry is unroutable as written -- add the route, or move the entry \
         to a provider that serves that dialect"
    )]
    ProviderMissingRoute {
        path: String,
        provider: String,
        model: String,
        dialect: &'static str,
        field: &'static str,
    },
    #[error(
        "catalog `{path}`: provider `{provider}` has base_url `{base_url}`, which names no \
         scheme; a request to it never leaves this process"
    )]
    ProviderBaseUrl {
        path: String,
        provider: String,
        base_url: String,
    },
    #[error(
        "catalog `{path}`: provider `{provider}` has `routes.{field}` = `{route}`, which does \
         not begin with `/`; joined onto the base URL it would address a sibling of the API \
         rather than a route under it"
    )]
    ProviderRoutePath {
        path: String,
        provider: String,
        field: &'static str,
        route: String,
    },
    #[error(
        "catalog `{path}`: provider `{provider}` says its key lives in `{env}`, which is not \
         a name an environment can hold"
    )]
    ProviderAuthEnv {
        path: String,
        provider: String,
        env: String,
    },
}

impl CatalogConfig {
    pub fn from_json(json: &str, path: &str) -> Result<Self, CatalogError> {
        let config: Self = serde_json::from_str(json).map_err(|source| CatalogError::Parse {
            path: path.to_string(),
            source,
        })?;
        // An empty catalog is refused rather than accepted: with nothing to
        // route to, every turn terminates incomplete, and a deployment would
        // read that as a broken engine rather than as a config file it
        // mistyped.
        if config.models.is_empty() {
            return Err(CatalogError::Empty {
                path: path.to_string(),
            });
        }
        config.validate(path)?;
        Ok(config)
    }

    /// Refuse a catalog that cannot mean one thing.
    ///
    /// This is the boundary the whole "one rate card" argument rests on. Both
    /// halves of the process resolve a model identity, and they do it
    /// differently — `CacheLedger::register` inserts into a map, so the last
    /// entry wins, while `MetricsConfig::rate_card` searches a list, so the
    /// first does. Reconciling the two lookups instead would be the wrong fix:
    /// it would pick a winner on the operator's behalf and leave an ambiguous
    /// file accepted. Making the ambiguity unrepresentable is what keeps the
    /// stated invariant true rather than merely usually true.
    ///
    /// Every check here is about a value that changes a dollar figure or gates
    /// a comparison. Non-finite prices are deliberately absent: JSON has no
    /// `NaN` literal and `serde_json` refuses a float it cannot represent, so
    /// parsing has already rejected them and a guard here would be dead code
    /// dressed as diligence.
    fn validate(&self, path: &str) -> Result<(), CatalogError> {
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        for spec in &self.models {
            if !seen.insert((spec.provider.as_str(), spec.model.as_str())) {
                return Err(CatalogError::DuplicateModel {
                    path: path.to_string(),
                    provider: spec.provider.clone(),
                    model: spec.model.clone(),
                });
            }

            let label = format!("{}/{}", spec.provider, spec.model);
            let rates = [
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
                ("base_ttft_ms", spec.base_ttft_ms),
                (
                    "ttft_ms_per_uncached_token",
                    spec.ttft_ms_per_uncached_token,
                ),
            ];
            for (field, value) in rates {
                // A negative rate does not merely mis-price: it reports the
                // fleet as having been *paid* to serve traffic, which reads on
                // the dashboard as an enormous saving.
                if value < 0.0 {
                    return Err(CatalogError::InvalidValue {
                        path: path.to_string(),
                        model: label.clone(),
                        field,
                        value,
                        expected: "rates and latencies cannot be negative",
                    });
                }
            }
            unit_interval(path, &label, "quality_prior", spec.quality_prior)?;
        }

        // Every definition judged before any entry is resolved against it, so
        // a malformed provider is reported as a malformed provider rather than
        // as the first entry that happened to name it.
        for (name, provider) in &self.providers {
            provider.validate(path, name)?;
        }

        // --- the two cross-checks that make the client registry total --------
        //
        // Together they are what lets `main`'s registry answer every provider
        // name a routing decision can produce. Without them an unknown provider
        // is discovered at dispatch — inside one tenant's turn, after the
        // decision has been written to their log — which is the shape
        // `frontier_client`'s load-or-die posture was written to avoid at the
        // process level and this is the same argument one layer down.
        //
        // What is deliberately *not* checked here: whether this build has a
        // transport that speaks the dialect. That is a fact about the binary,
        // not about the file, and it is checked where the binary is composed —
        // see `frontier_clients` in `main`. A boundary that asked it would
        // refuse a perfectly good catalog on a build that simply has fewer
        // clients compiled in, and would have to be edited every time one is
        // added.
        for spec in &self.models {
            if spec.provider != BUILT_IN_OPENAI && !self.providers.contains_key(&spec.provider) {
                return Err(CatalogError::UndefinedProvider {
                    path: path.to_string(),
                    provider: spec.provider.clone(),
                    model: spec.model.clone(),
                });
            }
            let Some(provider) = self.providers.get(&spec.provider) else {
                // The implicit `openai` provider, whose routes are the ones
                // `OpenAiResponsesClient` has always had. Nothing to check:
                // there is no file entry an operator could have got wrong.
                continue;
            };
            if provider.routes.for_dialect(spec.wire_protocol).is_none() {
                return Err(CatalogError::ProviderMissingRoute {
                    path: path.to_string(),
                    provider: spec.provider.clone(),
                    model: spec.model.clone(),
                    dialect: spec.wire_protocol.wire_name(),
                    field: ProviderRoutes::field_for(spec.wire_protocol),
                });
            }
        }

        // A correlary naming a model that is not here degrades silently inside
        // `ShadowPricing::resolve` — the local model is reported unpriced, and
        // the reason names a rate card nobody notices is missing.
        for correlary in &self.correlaries {
            let known = self
                .models
                .iter()
                .any(|m| m.provider == correlary.provider && m.model == correlary.model);
            if !known {
                return Err(CatalogError::UnknownCorrelaryTarget {
                    path: path.to_string(),
                    local_model: correlary.local_model.clone(),
                    provider: correlary.provider.clone(),
                    model: correlary.model.clone(),
                });
            }
        }

        // The gate's own inputs, on the same 0.0..=1.0 scale it compares.
        unit_interval(path, "<catalog>", "capability_band", self.capability_band)?;
        unit_interval(
            path,
            "<catalog>",
            "default_local_quality",
            self.default_local_quality,
        )?;
        for (model, prior) in &self.local_quality {
            unit_interval(path, model, "local_quality", *prior)?;
        }
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let display = path.display().to_string();
        let json = std::fs::read_to_string(path).map_err(|source| CatalogError::Read {
            path: display.clone(),
            source,
        })?;
        Self::from_json(&json, &display)
    }

    pub fn catalog(&self) -> StaticFrontierCatalog {
        StaticFrontierCatalog::new(self.models.clone())
    }

    /// The rate card and correlaries, for reporting.
    ///
    /// Built from the same [`Self::catalog`] the router uses — see
    /// `StaticFrontierCatalog::shadow_pricing` — so there is one set of prices
    /// in the process, not two.
    pub fn metrics_config(&self) -> MetricsConfig {
        let mut pricing = self
            .catalog()
            .shadow_pricing()
            .with_capability_band(self.capability_band);
        for correlary in &self.correlaries {
            pricing = pricing.declare(
                &correlary.local_model,
                &correlary.provider,
                &correlary.model,
                &correlary.note,
            );
        }
        let mut config =
            MetricsConfig::new(pricing).with_default_local_quality(self.default_local_quality);
        for (model, prior) in &self.local_quality {
            config = config.with_local_quality(model, *prior);
        }
        config
    }
}

/// The catalog named by [`CATALOG_VAR`], or `None` if the variable is unset.
///
/// A variable that *is* set but names an unreadable or malformed file is an
/// error rather than a fallback. Starting anyway would serve every turn under
/// prices the operator did not choose and report savings against them, which is
/// the one failure this whole module exists to prevent.
pub fn from_env() -> Result<Option<CatalogConfig>, CatalogError> {
    match std::env::var(CATALOG_VAR) {
        Ok(path) if !path.trim().is_empty() => CatalogConfig::load(path.trim()).map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roundhouse_core::metrics::{Correlary, PricedBasis};

    const SAMPLE: &str = r#"{
      "providers": {
        "anthropic": {
          "base_url": "https://api.anthropic.test/v1",
          "routes": { "messages": "/messages" },
          "auth": { "env": "ANTHROPIC_API_KEY" }
        }
      },
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
        }
      ],
      "correlaries": [
        {
          "local_model": "llama",
          "provider": "anthropic",
          "model": "claude-sonnet",
          "note": "within 2 points on our internal eval"
        }
      ],
      "local_quality": { "llama": 0.62 },
      "capability_band": 0.05
    }"#;

    #[test]
    fn a_catalog_configures_the_router_and_the_dashboard_from_one_rate_card() {
        let config = CatalogConfig::from_json(SAMPLE, "test").unwrap();
        let catalog = config.catalog();
        assert_eq!(catalog.models().len(), 1);

        let metrics = config.metrics_config();
        let reference = &metrics.pricing.references()[0];
        assert_eq!(
            reference.pricing,
            catalog.models()[0].pricing,
            "the price the dashboard reports must be the price the router chose on"
        );
        assert_eq!(metrics.pricing.capability_band(), 0.05);
    }

    #[test]
    fn a_declared_correlary_survives_into_the_metrics_config() {
        let config = CatalogConfig::from_json(SAMPLE, "test").unwrap();
        let metrics = config.metrics_config();

        let correlary = metrics
            .pricing
            .resolve("llama", 0.62, None, &HashMap::new());
        assert_eq!(correlary.reference().unwrap().model, "claude-sonnet");
        match &correlary {
            Correlary::Priced {
                basis: PricedBasis::Declared { note },
                ..
            } => assert!(note.contains("internal eval"), "the note is shown verbatim"),
            other => panic!("expected a declared basis, got {other:?}"),
        }
    }

    #[test]
    fn optional_fields_fall_back_to_documented_defaults() {
        let minimal = r#"{
          "models": [{
            "provider": "openai",
            "model": "gpt",
            "wire_protocol": "openai_chat_completions",
            "cache_model": {
              "kind": "inactivity_decay",
              "half_life_ms": 300000,
              "max_ttl_ms": 3600000,
              "min_prefix_tokens": 1024
            },
            "pricing": {
              "input_per_mtok_usd": 1.0,
              "cached_input_per_mtok_usd": 0.1,
              "cache_write_per_mtok_usd": 0.0,
              "output_per_mtok_usd": 4.0
            },
            "quality_prior": 0.7,
            "base_ttft_ms": 300.0,
            "ttft_ms_per_uncached_token": 0.001
          }]
        }"#;
        let config = CatalogConfig::from_json(minimal, "test").unwrap();
        assert!(config.correlaries.is_empty());
        assert_eq!(config.capability_band, DEFAULT_CAPABILITY_BAND);
        assert_eq!(config.default_local_quality, 0.5);
    }

    #[test]
    fn an_empty_catalog_is_refused_rather_than_started_with() {
        let error = CatalogConfig::from_json(r#"{ "models": [] }"#, "test").unwrap_err();
        assert!(matches!(error, CatalogError::Empty { .. }));
    }

    /// One entry, parameterized on the two fields the provider cross-checks
    /// read, and nothing else — so a refusal below is unambiguously about the
    /// provider and not about a price or a prior.
    fn one_entry(providers: &str, provider: &str, wire_protocol: &str) -> String {
        format!(
            r#"{{
              "providers": {providers},
              "models": [{{
                "provider": "{provider}",
                "model": "flagship",
                "wire_protocol": "{wire_protocol}",
                "cache_model": {{ "kind": "deterministic", "ttl_ms": 300000 }},
                "pricing": {{
                  "input_per_mtok_usd": 1.0,
                  "cached_input_per_mtok_usd": 0.1,
                  "cache_write_per_mtok_usd": 0.0,
                  "output_per_mtok_usd": 4.0
                }},
                "quality_prior": 0.7,
                "base_ttft_ms": 300.0,
                "ttft_ms_per_uncached_token": 0.001
              }}]
            }}"#
        )
    }

    /// **P1/P2's boot cross-check, the config half.**
    ///
    /// A `provider` string is what the client registry is keyed by, so an
    /// entry naming one nothing defines is a routing decision with no transport
    /// behind it. Refusing it here — at load, before a session exists — is what
    /// makes `an_unknown_provider_is_refused_at_boot_not_at_first_dispatch`
    /// true of the whole process rather than of one composition site.
    #[test]
    fn an_entry_naming_an_undefined_provider_is_refused_at_load() {
        let error =
            CatalogConfig::from_json(&one_entry("{}", "openrouter", "openai_responses"), "test")
                .expect_err("a provider nothing defines has no client to dispatch through");
        assert!(
            matches!(&error, CatalogError::UndefinedProvider { provider, .. }
                if provider == "openrouter"),
            "{error}"
        );
        // And the refusal points at the two ways out, because an operator
        // holding it is deciding between them.
        let message = error.to_string();
        assert!(
            message.contains("\"providers\"") && message.contains("openai"),
            "{message}"
        );

        // CONTROL 1: the same entry with the definition present validates, so
        // the refusal is about the missing definition and not about the name.
        CatalogConfig::from_json(
            &one_entry(
                r#"{ "openrouter": { "base_url": "https://openrouter.ai/api/v1",
                     "routes": { "responses": "/responses" },
                     "auth": { "env": "OPENROUTER_API_KEY" } } }"#,
                "openrouter",
                "openai_responses",
            ),
            "test",
        )
        .expect("a defined provider is routable");

        // CONTROL 2: the implicit `openai` provider still needs no section at
        // all. This is the backward-compatibility promise in executable form —
        // every catalog written before M10.1 is exactly this shape.
        CatalogConfig::from_json(&one_entry("{}", "openai", "openai_responses"), "test")
            .expect("the built-in provider needs no definition");
    }

    /// A definition that cannot carry one of its own entries.
    ///
    /// The failure this prevents is quiet in the worst way: the provider
    /// exists, the client is built, the request is serialized — and there is no
    /// path to POST it to, so the entry is unroutable for reasons that look
    /// like an outage at the far end.
    #[test]
    fn a_provider_with_no_route_for_its_entrys_dialect_is_refused_at_load() {
        let responses_only = r#"{ "openrouter": {
            "base_url": "https://openrouter.ai/api/v1",
            "routes": { "responses": "/responses" },
            "auth": { "env": "OPENROUTER_API_KEY" } } }"#;

        let error = CatalogConfig::from_json(
            &one_entry(responses_only, "openrouter", "anthropic_messages"),
            "test",
        )
        .expect_err("a dialect with no route has nowhere to be sent");
        assert!(
            matches!(
                &error,
                CatalogError::ProviderMissingRoute { dialect, field, .. }
                    if *dialect == "anthropic_messages" && *field == "messages"
            ),
            "{error}"
        );
        // Named the way the file spells it, so the remedy is a field an
        // operator can find rather than a dialect they already wrote.
        assert!(error.to_string().contains("routes.messages"), "{error}");

        // CONTROL: the identical provider serving the identical entry over the
        // dialect it *did* declare. One field different, and it validates —
        // which is what makes the refusal above about the route rather than
        // about OpenRouter or about `anthropic_messages`.
        CatalogConfig::from_json(
            &one_entry(responses_only, "openrouter", "openai_responses"),
            "test",
        )
        .expect("the declared dialect is routable");
    }

    #[test]
    fn a_malformed_catalog_names_the_file_it_could_not_parse() {
        let error = CatalogConfig::from_json("{ not json", "/etc/roundhouse.json").unwrap_err();
        assert!(error.to_string().contains("/etc/roundhouse.json"));
    }
}
