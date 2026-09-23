// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `catalog_config` under test.

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
        .resolve("llama", 0.62, None, &HashMap::new(), None);
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

/// One entry, parameterized on the four fields the cache-write guard reads.
///
/// The provider is a parameter because the guard is scoped by dialect and
/// not by branding: the same Claude model is reachable as `anthropic` and
/// through a gateway under any name an operator picks, and a check keyed on
/// the name would pass the gateway spelling.
fn one_cached_entry(
    provider: &str,
    wire_protocol: &str,
    ttl_ms: u64,
    input: f64,
    cache_write: f64,
) -> String {
    format!(
        r#"{{
          "providers": {{ "{provider}": {{
            "base_url": "https://gateway.test/v1",
            "routes": {{ "messages": "/messages", "responses": "/responses" }},
            "auth": {{ "env": "GATEWAY_KEY" }}
          }} }},
          "models": [{{
            "provider": "{provider}",
            "model": "claude-sonnet",
            "wire_protocol": "{wire_protocol}",
            "cache_model": {{ "kind": "deterministic", "ttl_ms": {ttl_ms} }},
            "pricing": {{
              "input_per_mtok_usd": {input},
              "cached_input_per_mtok_usd": 0.3,
              "cache_write_per_mtok_usd": {cache_write},
              "output_per_mtok_usd": 15.0
            }},
            "quality_prior": 0.62,
            "base_ttft_ms": 350.0,
            "ttft_ms_per_uncached_token": 0.002
          }}]
        }}"#
    )
}

/// An hour-long entry priced as if it were a five-minute one is refused.
///
/// The provider bills a one-hour write at twice the input rate and a
/// five-minute write at 1.25 times. Both numbers come from the same field,
/// so an entry that asks for the hour and carries the cheaper rate
/// under-reports the cost of every write it makes — and the dashboard
/// publishes the difference as a saving.
#[test]
fn a_one_hour_cache_model_requires_the_one_hour_write_rate() {
    let error = CatalogConfig::from_json(
        &one_cached_entry("anthropic", "anthropic_messages", 3_600_000, 3.0, 3.75),
        "test",
    )
    .expect_err("3.75 is the five-minute rate on a three-dollar input");
    assert!(
        matches!(&error, CatalogError::OneHourWriteRate { declared, required, .. }
            if *declared == 3.75 && *required == 6.0),
        "{error}"
    );
    // The number to write, not just the diagnosis: an operator holding this
    // in a boot log is deciding what to put in the file.
    assert!(error.to_string().contains('6'), "{error}");

    // CONTROL 1: the same entry at twice input loads. One field different,
    // so the refusal is about the rate and not about the hour.
    CatalogConfig::from_json(
        &one_cached_entry("anthropic", "anthropic_messages", 3_600_000, 3.0, 6.0),
        "test",
    )
    .expect("twice the input rate is what an hour costs");

    // CONTROL 2: the five-minute entry with the same 3.75 the refusal
    // above rejected. The guard is about the hour, not about the ratio.
    CatalogConfig::from_json(
        &one_cached_entry("anthropic", "anthropic_messages", 300_000, 3.0, 3.75),
        "test",
    )
    .expect("a five-minute write is priced at 1.25 times and is not this check's business");

    // CONTROL 3: the all-zero placeholder every example and the offline
    // stub ship. Zero is twice zero, so a catalog nobody has priced yet
    // still loads — this is what keeps `catalog.example.json` valid.
    CatalogConfig::from_json(
        &one_cached_entry("anthropic", "anthropic_messages", 3_600_000, 0.0, 0.0),
        "test",
    )
    .expect("an unpriced placeholder catalog must still load");
}

/// The guard follows the dialect, so a gateway cannot spell its way out.
///
/// `openrouter-messages` is the second definition the shipped example
/// demonstrates: one provider name, someone else's model, the same wire. A
/// check keyed on `provider == "anthropic"` would let exactly this entry
/// declare an hour at the cheaper rate.
#[test]
fn the_write_rate_guard_follows_the_dialect_rather_than_the_provider_name() {
    let error = CatalogConfig::from_json(
        &one_cached_entry(
            "openrouter-messages",
            "anthropic_messages",
            3_600_000,
            3.0,
            3.75,
        ),
        "test",
    )
    .expect_err("the dialect decides, not the name above it");
    assert!(
        matches!(&error, CatalogError::OneHourWriteRate { model, .. }
            if model.starts_with("openrouter-messages/")),
        "{error}"
    );
}

/// **CONTROL.** Other dialects are untouched by this rung.
///
/// A `deterministic` hour on a Responses entry places no `cache_control`
/// marker anywhere — that vocabulary is this one dialect's — so pricing it
/// against Anthropic's multiplier would refuse a catalog for a rule its
/// provider never published.
#[test]
fn a_non_messages_dialect_is_not_held_to_the_one_hour_write_rate() {
    CatalogConfig::from_json(
        &one_cached_entry("openrouter", "openai_responses", 3_600_000, 3.0, 3.75),
        "test",
    )
    .expect("another dialect's write pricing is not this check's to assert");
}

/// **CORRECTNESS (fleet-redis-2).** Anthropic's wire has exactly two cache
/// lifetimes: the five-minute default and an explicit one-hour marker. A
/// `deterministic` entry at any other TTL passed this boundary today,
/// `body()` fell silently back to the five-minute default (no `ttl` field at
/// all), and the ledger kept modelling the target as warm for the number the
/// catalog declared -- pricing a cache hit the wire was never asked to grant.
#[test]
fn a_deterministic_ttl_the_wire_has_no_spelling_for_is_refused() {
    let error = CatalogConfig::from_json(
        &one_cached_entry("anthropic", "anthropic_messages", 600_000, 3.0, 3.75),
        "test",
    )
    .expect_err(
        "600000ms is neither Anthropic's five-minute default nor its one-hour marker, \
         so the wire can never honor it",
    );
    assert!(
        matches!(&error, CatalogError::UnsupportedCacheLifetime { ttl_ms, .. } if *ttl_ms == 600_000),
        "{error}"
    );

    // CONTROL: the guard follows the dialect, not the provider name -- a
    // gateway entry speaking `anthropic_messages` under any name is held to
    // the same rule the write-rate guard above is.
    let error = CatalogConfig::from_json(
        &one_cached_entry(
            "openrouter-messages",
            "anthropic_messages",
            600_000,
            3.0,
            3.75,
        ),
        "test",
    )
    .expect_err("the dialect decides, not the name above it");
    assert!(
        matches!(&error, CatalogError::UnsupportedCacheLifetime { model, .. }
            if model.starts_with("openrouter-messages/")),
        "{error}"
    );

    // CONTROL: another dialect's own TTL semantics are not this check's
    // business -- an `openai_responses` entry has no `cache_control`
    // vocabulary to be held to at all.
    CatalogConfig::from_json(
        &one_cached_entry("openrouter", "openai_responses", 600_000, 3.0, 3.75),
        "test",
    )
    .expect("a non-Messages dialect may declare whatever TTL its own cache model means");
}

/// One minimal entry, parameterized on whatever local section is under
/// test, so a refusal below is unambiguously about the local numbers.
fn with_local_section(local: &str) -> String {
    format!(
        r#"{{
          "models": [{{
            "provider": "openai",
            "model": "gpt",
            "wire_protocol": "openai_responses",
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
          }}]{local}
        }}"#
    )
}

/// **CONTROL.** A file that says nothing about local latency leaves the
/// engine exactly where it was.
///
/// Asserted against `EngineConfig::default` rather than against `60.0`:
/// the claim is that the two agree, and a literal here would pass on the
/// day they stopped agreeing — which is the only way this field can change
/// a deployment's quotes without anyone editing a file.
#[test]
fn a_catalog_with_no_local_section_quotes_the_engine_defaults() {
    let config = CatalogConfig::from_json(&with_local_section(""), "test").unwrap();
    assert_eq!(
        config.local_base_ttft_ms,
        EngineConfig::default().local_base_ttft_ms,
    );
    assert_eq!(
        config.local_ttft_ms_per_prefill_token, 0.0,
        "an unmeasured slope must quote flat rather than guess"
    );

    let engine = engine_config(Some(&config));
    assert_eq!(
        engine.local_base_ttft_ms,
        EngineConfig::default().local_base_ttft_ms
    );
    assert_eq!(engine.local_ttft_ms_per_prefill_token, 0.0);
}

/// **CONTROL.** No catalog at all is the offline-stub deployment, and it
/// runs on the same documented defaults.
#[test]
fn no_catalog_leaves_every_engine_number_at_its_default() {
    let engine = engine_config(None);
    assert_eq!(
        engine.local_base_ttft_ms,
        EngineConfig::default().local_base_ttft_ms
    );
    assert_eq!(engine.local_ttft_ms_per_prefill_token, 0.0);
}

/// The measured numbers reach the engine config, which is the whole point
/// of the field existing.
#[test]
fn a_measured_local_curve_reaches_the_engine_config() {
    let config = CatalogConfig::from_json(
        &with_local_section(
            r#",
          "local_base_ttft_ms": 90.0,
          "local_ttft_ms_per_prefill_token": 0.25"#,
        ),
        "test",
    )
    .unwrap();

    let engine = engine_config(Some(&config));
    assert_eq!(
        engine.local_base_ttft_ms, 90.0,
        "the deployment's measured floor, not the built-in one"
    );
    assert_eq!(
        engine.local_ttft_ms_per_prefill_token, 0.25,
        "the deployment's measured prefill rate, 1000 / tokens_per_second"
    );
}

/// A negative latency is refused for the same reason a negative rate is.
///
/// A negative slope does not merely mis-quote: it makes a local worker look
/// *faster* the more it has to prefill, so the router hands a long cold
/// context to the one target no provider bill ever contradicts, and the
/// dashboard reports the miss as a saving.
#[test]
fn a_negative_local_latency_is_refused_at_load() {
    for (field, section) in [
        (
            "local_base_ttft_ms",
            r#","local_base_ttft_ms": -1.0"#.to_string(),
        ),
        (
            "local_ttft_ms_per_prefill_token",
            r#","local_ttft_ms_per_prefill_token": -0.25"#.to_string(),
        ),
    ] {
        let error = CatalogConfig::from_json(&with_local_section(&section), "test")
            .expect_err("a negative latency quotes a worker as faster for being colder");
        assert!(
            matches!(&error, CatalogError::InvalidValue { field: named, .. }
                if *named == field),
            "{field}: {error}"
        );
    }

    // CONTROL: zero is not negative, and it is the documented default for
    // the slope — so the refusal above is about the sign and not about the
    // field being set at all.
    CatalogConfig::from_json(
        &with_local_section(
            r#",
          "local_base_ttft_ms": 0.0,
          "local_ttft_ms_per_prefill_token": 0.0"#,
        ),
        "test",
    )
    .expect("a floor of zero and an unmeasured slope are both sayable");
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

/// **Thermo-nuclear review G15, reachability half.** The boundary refuses
/// a `provider` naming nothing, and it refuses a provider missing a route
/// for its entry's dialect — but nothing here refuses a `providers` map
/// that explicitly redefines the key `openai`. That means the scenario
/// `an_explicit_openai_definition_says_it_is_taking_over_from_the_variables`
/// (in `main.rs`) exercises is one an operator can actually reach through
/// a parsed catalog file, not just through `frontier_clients` called
/// directly: nothing here stops them writing `"providers": {"openai":
/// {...}}` next to `ROUNDHOUSE_OPENAI_API_BASE` and getting no refusal, no
/// warning, and a silently shadowed variable.
#[test]
fn an_explicit_openai_provider_entry_validates_unremarked() {
    CatalogConfig::from_json(
        &one_entry(
            r#"{ "openai": { "base_url": "https://openai-relay.internal/v1",
                 "routes": { "responses": "/responses" },
                 "auth": { "env": "OPENAI_RELAY_KEY" } } }"#,
            "openai",
            "openai_responses",
        ),
        "test",
    )
    .expect(
        "the config boundary has no check that would refuse a `providers.openai` entry, \
         which is what makes the shadowing in `frontier_clients` reachable from a file an \
         operator actually writes rather than only from a hand-built HashMap",
    );
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

/// G17 (M10 review): the `catalog.example.json` `$comment` spends eleven
/// lines warning that a tilde-alias (`~deepseek/deepseek-v4-flash-latest`)
/// is a rolling pointer OpenRouter may re-point at any time, and that the
/// catalog "has no mechanism to re-resolve it later" — but `validate`
/// never inspects the shape of `spec.model` at all, so that discipline is
/// prose, not a check. This asserts the load refuses a tilde-alias id,
/// which is what "has no mechanism to re-resolve it later" has to mean if
/// the rule mattered enough to state.
///
/// **Refusal, provider-scoped.** The ruling on G17 was between refuse, warn
/// and accept; refuse is what the three neighbouring identity checks already
/// do, and the scope is `openrouter` alone because `~` is a marker in that
/// provider's id vocabulary and nobody else's — see
/// [`ROLLING_ALIAS_PROVIDER`]. The controls below are what keep this from
/// becoming a blanket shape rule over every provider's ids.
#[test]
fn a_rolling_alias_is_named_at_load() {
    /// One entry, parameterized on the two fields this check reads. The
    /// definition below is named after whichever provider the entry claims,
    /// so a provider rename moves both halves together and the controls
    /// differ from the probe in exactly the string under test.
    fn one_model(provider: &str, model: &str) -> String {
        format!(
            r#"{{
              "providers": {{ "{provider}": {{
                "base_url": "https://openrouter.ai/api/v1",
                "routes": {{ "responses": "/responses" }},
                "auth": {{ "env": "OPENROUTER_API_KEY" }}
              }} }},
              "models": [{{
                "provider": "{provider}",
                "model": "{model}",
                "wire_protocol": "openai_responses",
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

    let error = CatalogConfig::from_json(
        &one_model("openrouter", "~deepseek/deepseek-v4-flash-latest"),
        "test",
    )
    .expect_err(
        "a rolling-pointer model id mis-prices every turn after OpenRouter re-points it, \
         same as a duplicate identity or an off-scale prior",
    );
    assert!(
        matches!(
            &error,
            CatalogError::RollingModelAlias { model, .. }
                if model == "~deepseek/deepseek-v4-flash-latest"
        ),
        "{error}"
    );
    // The remedy in the message, not just the diagnosis: an operator reads
    // this in a boot log and needs the id to paste and then date.
    assert!(
        error
            .to_string()
            .contains("deepseek/deepseek-v4-flash-latest`"),
        "the refusal must end in the id with the marker gone: {error}"
    );

    // CONTROL 1: the same provider with the full dated id the example's own
    // `$comment` tells an operator to write. One character different, and it
    // loads — which is what makes the refusal about the alias marker rather
    // than about OpenRouter or about slashes in an id.
    CatalogConfig::from_json(
        &one_model("openrouter", "deepseek/deepseek-v4-flash-0731"),
        "test",
    )
    .expect("a frozen dated snapshot is exactly what this check is asking for");

    // CONTROL 2: the identical alias-shaped id under a provider that is not
    // OpenRouter. `~` means "whatever is newest" in one provider's id
    // vocabulary and is an ordinary character everywhere else, so refusing
    // it here would be this boundary inventing a rule for a file it cannot
    // read — see `ROLLING_ALIAS_PROVIDER`.
    CatalogConfig::from_json(
        &one_model("some-other-gateway", "~deepseek/deepseek-v4-flash-latest"),
        "test",
    )
    .expect("another provider's ids are opaque strings and not ours to shape-check");
}
