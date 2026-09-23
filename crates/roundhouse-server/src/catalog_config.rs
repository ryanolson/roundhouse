// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Deployment configuration for the catalog, the rate card, the correlaries,
//! and the local latency curve they are all compared against.
//!
//! One file, because these are one fact seen from several angles. The catalog
//! is what the router may choose between; the rate card is what those choices
//! cost; the correlaries are what our own models stand in for when they are
//! priced; the local TTFT curve is the fourth axis of that same comparison,
//! and it is here because every hosted entry already carries its own
//! `base_ttft_ms` and `ttft_ms_per_uncached_token` — a deployment that
//! configured the local side somewhere else would be writing the two halves of
//! one comparison in two files. Splitting them would let the price the router
//! optimizes against drift from the price the dashboard reports saving, and
//! those two numbers disagreeing is worse than either being wrong — it is
//! unfalsifiable.
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
use roundhouse_fleet::anthropic_messages::CacheLifetime;
use roundhouse_fleet::{CacheLifetimeError, FrontierModelSpec, StaticFrontierCatalog};

use crate::engine::{DEFAULT_LOCAL_BASE_TTFT_MS, EngineConfig};

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
    /// Local latency floor in milliseconds. Uses the engine's default when omitted.
    #[serde(default = "default_local_base_ttft_ms")]
    pub local_base_ttft_ms: f64,
    /// Milliseconds per effective prefill token, measured as `1000 / tokens_per_second`.
    ///
    /// Zero leaves the quote flat until the deployment has a prefill measurement.
    /// Keeping local and hosted latency values here makes their comparison
    /// inspectable in the same deployment configuration.
    #[serde(default)]
    pub local_ttft_ms_per_prefill_token: f64,
    /// The citation for imported `quality_prior`s, if a provenance file was
    /// found beside this catalog. Never read from the catalog JSON itself —
    /// see [`quality_prior_citation`].
    #[serde(skip)]
    pub quality_prior_citation: Option<String>,
}

/// What `import-benchmarks` writes beside the fragment, under its default name.
const PROVENANCE_FILE: &str = "quality-prior.provenance.json";

/// The attribution for imported quality priors, discovered beside the catalog.
///
/// **Why the server reads a file nobody named it.** `import-benchmarks` sources
/// `quality_prior` from OpenRouter's published index, which requires
/// attribution when the data is republished — and roundhouse republishes
/// figures derived from it: the savings dashboard's routing saving is priced
/// through the capability gate those priors feed. The fragment the tool emits
/// cannot carry the attribution (a catalog entry is `deny_unknown_fields`, and
/// inventing a field there would put somebody else's schema into every
/// catalog), so the obligation lives in the paired provenance file. Reading it
/// here is what turns "keep the two files together" from an instruction into
/// something the deployment does on the operator's behalf (M10 review G12).
///
/// **The convention is the tool's own default filename**, `--provenance`'s
/// default and what the README tells an operator to keep beside the catalog. A
/// deployment that renamed the file on the command line is not discovered and
/// gets no line — deliberately, since guessing at other names would mean
/// parsing every JSON file in the catalog's directory to see whether it looks
/// like provenance, and a heuristic that reads unrelated files is a worse
/// answer than a convention written down in two places.
///
/// **Discovered, therefore never fatal.** [`CatalogConfig::load`] is
/// load-or-die because an operator named that path; nothing named this one, so
/// a missing file yields no citation and no complaint, and a malformed or
/// unattributed one yields a warning and no citation. The alternative — a parse
/// error here stopping the process — would let a stray file in a directory take
/// a deployment down, which is a much worse failure than an uncited figure.
fn quality_prior_citation(catalog_path: &Path) -> Option<String> {
    let path = catalog_path.parent()?.join(PROVENANCE_FILE);
    let json = std::fs::read_to_string(&path).ok()?;
    let document: serde_json::Value = match serde_json::from_str(&json) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "a quality-prior provenance file sits beside the catalog but does not parse;                  the savings figure will be published without its citation"
            );
            return None;
        }
    };
    let meta = &document["meta"];
    // The response's own citation when it named one. When it did not — the
    // ordinary multi-source case — the schema says to attribute per item, so
    // the line names the publishers the entries were attributed to instead.
    // Deduplicated and ordered by first appearance rather than sorted: the
    // provenance file's own entry order is what an operator reading the file
    // beside the dashboard sees.
    let attribution = match meta["citation"].as_str().filter(|c| !c.trim().is_empty()) {
        Some(citation) => citation.to_string(),
        None => {
            let mut sources: Vec<&str> = Vec::new();
            for entry in document["entries"].as_array().into_iter().flatten() {
                if let Some(source) = entry["attribution"]["source"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    && !sources.contains(&source)
                {
                    sources.push(source);
                }
            }
            if sources.is_empty() {
                tracing::warn!(
                    path = %path.display(),
                    "the quality-prior provenance file beside the catalog carries no citation                      and no per-entry source, so there is nothing to attribute with"
                );
                return None;
            }
            format!("attributed per source: {}", sources.join(", "))
        }
    };
    // The dataset's own version and date, appended when present: a citation
    // without them re-reads as current the day the upstream leaderboard moves,
    // which is exactly the failure `CLAUDE.md` asks an imported index to be
    // stamped against.
    let stamp = [meta["version"].as_str(), meta["as_of"].as_str()]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", as of ");
    Some(if stamp.is_empty() {
        attribution
    } else {
        format!("{attribution} ({stamp})")
    })
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

/// The one provider whose model ids carry a documented rolling-pointer form.
///
/// Scoped by provider name rather than applied to every entry, because `~` is
/// an alias marker only in OpenRouter's id vocabulary: every other provider's
/// model id is an opaque string, and a blanket shape rule would refuse a
/// legitimate id for resembling somebody else's convention (M10 review G17).
///
/// **The cost of the narrow rule, written down rather than discovered.** A
/// deployment that names its OpenRouter definition something else — `"or"`,
/// `"router"` — writes ids this check never reads, and the `$comment` in
/// `examples/catalog.example.json` stays their only defence. Widening it to
/// match on `base_url` was the alternative and is worse: the refusal would then
/// depend on a field the message does not name, so an operator who renamed the
/// provider would be told their model id is wrong by a check keyed on their URL.
const ROLLING_ALIAS_PROVIDER: &str = "openrouter";

/// What OpenRouter prefixes an id with when it means "whatever is newest".
const ROLLING_ALIAS_MARKER: char = '~';

fn default_local_quality() -> f64 {
    0.5
}

fn default_capability_band() -> f64 {
    DEFAULT_CAPABILITY_BAND
}

fn default_local_base_ttft_ms() -> f64 {
    DEFAULT_LOCAL_BASE_TTFT_MS
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
    /// Carries the computed rate so the load error names the required value.
    #[error(
        "catalog `{path}`: `{model}` declares a one-hour cache. \
         cache_write_per_mtok_usd must equal twice the input rate ({required}), got {declared}"
    )]
    OneHourWriteRate {
        path: String,
        model: String,
        declared: f64,
        required: f64,
    },
    /// A `deterministic` entry on `anthropic_messages` declares a TTL the wire
    /// has no spelling for.
    ///
    /// Anthropic offers exactly two lifetimes: the five-minute default (no
    /// `ttl` field) and an explicit one-hour marker. Anything else passed
    /// this boundary silently: `AnthropicMessagesClient::body` fell back to
    /// the default with no `ttl` at all, while `CacheLedger` kept modelling
    /// the target as warm for the number the catalog declared — the router
    /// pricing a cache hit the wire was never asked to grant.
    #[error(
        "catalog `{path}`: `{model}` declares a deterministic cache lifetime of {ttl_ms}ms on \
         `anthropic_messages`, which has no spelling for it on the wire -- only the \
         five-minute default (300000) and the explicit one-hour marker (3600000) exist. \
         Routing on this entry would price a cache hit the wire is never asked to grant"
    )]
    UnsupportedCacheLifetime {
        path: String,
        model: String,
        ttl_ms: u64,
    },
    /// An `inactivity_decay` entry on `anthropic_messages` models retention
    /// past what the wire's own undeclared default grants.
    ///
    /// Anthropic's Messages cache is deterministic, not automatic: past the
    /// silent five-minute default, the dialect's only lever is the explicit
    /// `1h` marker `CacheModel::Deterministic` spells, not a decay curve.
    /// A `max_ttl_ms` past the default therefore prices warmth this wire was
    /// never asked to grant, for the same reason a `deterministic` TTL it
    /// cannot spell is refused above (fleet-redis-r3-1).
    #[error(
        "catalog `{path}`: `{model}` models an automatic cache retained up to \
         {max_ttl_ms}ms on `anthropic_messages`, past the {default_ttl_ms}ms the wire \
         grants with no marker to ask for more. This dialect's cache is deterministic, \
         not automatic -- use `cache_model.kind: \"deterministic\"` with an explicit \
         `ttl_ms`, or lower `max_ttl_ms` to {default_ttl_ms} or below"
    )]
    UndeclaredCacheDecay {
        path: String,
        model: String,
        max_ttl_ms: u64,
        default_ttl_ms: u64,
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
        "catalog `{path}`: `{provider}/{model}` names a rolling pointer rather than a model. \
         `{provider}` re-points a `~`-prefixed alias whenever it likes, and this catalog has no \
         mechanism to re-resolve one: the price beside it was quoted for whatever the alias meant \
         the day it was written, so every turn after a re-point is dispatched on one model and \
         priced on another, and the dashboard reports the difference as a saving. Write the full \
         dated id you mean -- `{suggestion}` if that is still the model you want"
    )]
    RollingModelAlias {
        path: String,
        provider: String,
        model: String,
        /// The same id with the alias marker gone, so the message ends in
        /// something an operator can paste and then date, rather than in advice.
        suggestion: String,
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
    /// A spelling for a stored key that no client implements.
    ///
    /// Its own arm rather than a defaulted value, because the failure it
    /// prevents is invisible from every read surface afterwards: a key sent in
    /// the header its provider ignores authenticates as nobody, and the 401
    /// that comes back reads as a bad key rather than as a wrong file. The
    /// accepted set is rendered from the client's own enum, so a style added
    /// there cannot leave this message naming an incomplete list.
    #[error(
        "catalog `{path}`: provider `{provider}` says its key is spelled `{style}` in \
         `auth.style`, which is not a spelling this build sends; the accepted values are \
         {accepted}, and omitting the field means `x_api_key` -- Anthropic's own convention, \
         which is what a first-party Messages endpoint requires and what OpenRouter's \
         `/messages` route refuses"
    )]
    ProviderAuthStyle {
        path: String,
        provider: String,
        style: String,
        accepted: String,
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
    /// Every check here is about a value that changes a dollar figure, gates a
    /// comparison, or moves a route — the local latency curve is the third of
    /// those. Non-finite prices are deliberately absent: JSON has no
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

            // The fourth way one line of this file silently mis-prices a turn,
            // and until now the only one left to prose. A duplicate identity,
            // a negative rate and an off-scale prior are all refused here
            // because each resolves differently on the two sides of the
            // process; a rolling alias resolves consistently and then *moves*,
            // which is the same defect with a delay — the rate card stays
            // beside an id that no longer means what it meant, and the
            // dashboard reports the gap as a saving. Refused rather than
            // warned, for the reason `UndefinedProvider` is: the catalog is the
            // one place the ambiguity is still cheap to remove, and a warning
            // at load is read once and then scrolled past for the life of the
            // deployment.
            if spec.provider == ROLLING_ALIAS_PROVIDER
                && spec.model.starts_with(ROLLING_ALIAS_MARKER)
            {
                return Err(CatalogError::RollingModelAlias {
                    path: path.to_string(),
                    provider: spec.provider.clone(),
                    model: spec.model.clone(),
                    suggestion: spec
                        .model
                        .trim_start_matches(ROLLING_ALIAS_MARKER)
                        .to_string(),
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

            // `requested_cache_lifetime` is the one place "which deterministic
            // TTLs Anthropic's wire can spell, and how far an automatic decay
            // may model retention" is decided — its own doc says a variant
            // added to either `WireProtocol` or `CacheModel` fails to compile
            // there until someone decides what it means. Calling it here,
            // rather than re-deriving the same rule by hand, is what keeps
            // that compile-time protection real: a hand-written `==` copy
            // would not fail to compile on a new variant, it would silently
            // accept it (fleet-redis-r2-2 / server-r2-1). The match below is
            // over `CacheLifetimeError`'s own two variants rather than the
            // wide `FrontierError`, so it too is exhaustive -- a third
            // refusal the resolver grows fails to compile here instead of
            // panicking at boot through a wildcard arm (fleet-redis-r3-1).
            let lifetime = spec
                .requested_cache_lifetime()
                .map_err(|error| match error {
                    CacheLifetimeError::UnspellableTtl { ttl_ms } => {
                        CatalogError::UnsupportedCacheLifetime {
                            path: path.to_string(),
                            model: label.clone(),
                            ttl_ms,
                        }
                    }
                    CacheLifetimeError::UndeclaredDecay {
                        max_ttl_ms,
                        default_ttl_ms,
                    } => CatalogError::UndeclaredCacheDecay {
                        path: path.to_string(),
                        model: label.clone(),
                        max_ttl_ms,
                        default_ttl_ms,
                    },
                })?;

            // A single write rate must match the lifetime requested on the
            // wire. Only `anthropic_messages` plus `Deterministic{ONE_HOUR_MS}`
            // resolves to `OneHour` (see `requested_cache_lifetime`'s match),
            // so testing the resolved lifetime already scopes this to the
            // dialect that has an hour to ask for.
            let one_hour_write = 2.0 * spec.pricing.input_per_mtok_usd;
            if lifetime == CacheLifetime::OneHour
                && spec.pricing.cache_write_per_mtok_usd != one_hour_write
            {
                return Err(CatalogError::OneHourWriteRate {
                    path: path.to_string(),
                    model: label.clone(),
                    declared: spec.pricing.cache_write_per_mtok_usd,
                    required: one_hour_write,
                });
            }
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

        // The local half of the latency curve, held to the rule its hosted
        // half is held to above. A negative floor or slope does not merely
        // mis-quote: it makes a local worker look *faster* the more it has to
        // prefill, so the router hands its longest cold contexts to the one
        // target no provider bill ever arrives to contradict, and the
        // dashboard reports every miss as a saving.
        for (field, value) in [
            ("local_base_ttft_ms", self.local_base_ttft_ms),
            (
                "local_ttft_ms_per_prefill_token",
                self.local_ttft_ms_per_prefill_token,
            ),
        ] {
            if value < 0.0 {
                return Err(CatalogError::InvalidValue {
                    path: path.to_string(),
                    model: "<catalog>".to_string(),
                    field,
                    value,
                    expected: "rates and latencies cannot be negative",
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
        let mut config = Self::from_json(&json, &display)?;
        // Read here rather than in `from_json`: the citation is a fact about
        // where this file *is*, and `from_json` is the boundary tests hand a
        // string with no path behind it.
        config.quality_prior_citation = quality_prior_citation(path);
        Ok(config)
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
        // The attribution travels with the priors, into the one document that
        // republishes a figure derived from them.
        if let Some(citation) = &self.quality_prior_citation {
            config = config.with_quality_prior_citation(citation);
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

/// Apply the catalog's local latency values to the engine defaults.
///
/// The no-catalog path uses the same defaults as an omitted field. Keeping this
/// composition beside the loader lets tests exercise it without booting a server.
pub fn engine_config(config: Option<&CatalogConfig>) -> EngineConfig {
    let Some(config) = config else {
        return EngineConfig::default();
    };
    EngineConfig {
        local_base_ttft_ms: config.local_base_ttft_ms,
        local_ttft_ms_per_prefill_token: config.local_ttft_ms_per_prefill_token,
        ..EngineConfig::default()
    }
}

#[cfg(test)]
mod tests;
