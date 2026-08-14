// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Does the seam a `FrontierClient` sits on carry what usage enforcement needs?
//!
//! `usage.rs` states that `WireProtocol::enforce_usage_reporting` "is mandatory
//! for any `FrontierClient` speaking these protocols", and `FrontierModelSpec`
//! makes every catalog entry declare its `wire_protocol`. This exercises the
//! only argument a `FrontierClient` is actually handed — a `&FrontierQuote` —
//! and asks whether an implementer can discharge that obligation from it.
//!
//! It could not, and that was the defect: `Engine` holds *one*
//! `Arc<dyn FrontierClient>` "for a catalog of providers whose transports have
//! nothing in common", so a client per protocol was never the escape hatch it
//! looked like, and `target_alone_does_not_identify_a_dialect` below closes the
//! other one. `FrontierQuote` now carries the dialect, so the obligation can be
//! discharged from inside the trait.

use std::sync::Mutex;

use async_trait::async_trait;
use roundhouse_core::routing::{CacheLedger, CacheModel, ProviderPricing, Target};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierModelSpec, FrontierQuote, FrontierStream,
    StaticFrontierCatalog, WireProtocol,
};
use serde_json::{Value, json};

/// A catalog on the dialect that actually bites: a streaming OpenAI-compatible
/// request reports no usage at all unless it asked to.
fn chat_completions_catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: "openai".into(),
        model: "gpt-x".into(),
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        cache_model: CacheModel::Deterministic { ttl_ms: 300_000 },
        pricing: ProviderPricing {
            input_per_mtok_usd: 2.5,
            cached_input_per_mtok_usd: 0.25,
            cache_write_per_mtok_usd: 0.0,
            output_per_mtok_usd: 10.0,
        },
        quality_prior: 0.9,
        base_ttft_ms: 300.0,
        ttft_ms_per_uncached_token: 0.002,
    }])
}

/// The quote the engine builds for a frontier target, field for field
/// (`roundhouse-server/src/engine.rs`, the `Target::Frontier` arm).
fn quote_as_the_engine_builds_it(catalog: &StaticFrontierCatalog) -> FrontierQuote {
    let mut ledger = CacheLedger::new();
    catalog.apply_to_ledger(&mut ledger);
    let candidate = catalog.quote(&ledger, 0, 1_000, 500).remove(0);

    let spec = catalog
        .spec_for(&candidate.target)
        .expect("the catalog priced this target, so it owns its spec");

    FrontierQuote {
        wire_protocol: spec.wire_protocol,
        target: candidate.target,
        prompt: "how many tokens did that turn bill?".into(),
        prompt_cache_key: "sess_finding1".into(),
        expected_output_tokens: Some(512),
    }
}

/// What a client holding a quote can see of the dialect.
///
/// Now a plain field read. It stays a named function because the point of the
/// test below is that an implementer reaches this from `&FrontierQuote` alone,
/// with no catalog and no second source of truth in scope.
fn protocol_of(quote: &FrontierQuote) -> Option<WireProtocol> {
    Some(quote.wire_protocol)
}

/// The provider client the README lists under "Not yet built", reduced to the
/// one thing it must do before it sends anything: serialize a streaming request
/// and make the provider account for the call.
#[derive(Default)]
struct SerializingFrontierClient {
    last_body: Mutex<Option<Value>>,
}

#[async_trait]
impl FrontierClient for SerializingFrontierClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let Target::Frontier { model, .. } = &quote.target else {
            return Err(FrontierError::UnknownProvider(format!(
                "{:?}",
                quote.target
            )));
        };
        let mut body = json!({
            "model": model,
            "stream": true,
            "prompt_cache_key": quote.prompt_cache_key,
            "max_output_tokens": quote.expected_output_tokens,
            "messages": [{ "role": "user", "content": quote.prompt }],
        });

        // The mandatory step. It needs a `WireProtocol`, and the only thing in
        // scope to derive one from is the quote.
        if let Some(protocol) = protocol_of(quote) {
            protocol.enforce_usage_reporting(&mut body);
        }

        *self.last_body.lock().unwrap() = Some(body);
        Ok(FrontierChunk::whole_response("ok".into(), 0, 0, 0, 0))
    }
}

#[test]
fn the_quote_a_frontier_client_receives_carries_the_wire_protocol() {
    let catalog = chat_completions_catalog();
    let quote = quote_as_the_engine_builds_it(&catalog);

    assert_eq!(
        catalog.models()[0].wire_protocol,
        WireProtocol::OpenAiChatCompletions,
        "the catalog entry declares a dialect",
    );
    assert_eq!(
        protocol_of(&quote),
        Some(WireProtocol::OpenAiChatCompletions),
        "the quote handed to a FrontierClient must carry the dialect the \
         catalog declared; quote was: {quote:?}",
    );
}

#[tokio::test]
async fn a_frontier_client_asks_the_provider_to_report_usage() {
    let catalog = chat_completions_catalog();
    let quote = quote_as_the_engine_builds_it(&catalog);

    let client = SerializingFrontierClient::default();
    let _stream = client.execute(&quote).await.unwrap();

    let body = client.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["stream"], json!(true), "this request streams");
    assert_eq!(
        body["stream_options"]["include_usage"],
        json!(true),
        "a streaming OpenAI-compatible request that never asked for usage comes \
         back with none, and folds into the dashboard as zero tokens for zero \
         dollars; body was {body}",
    );
}

/// Control: the enforcement itself is correct, and applies to exactly the body
/// the client above builds — the moment something supplies the protocol.
#[test]
fn enforcement_works_the_instant_the_protocol_is_known() {
    let mut body = json!({
        "model": "gpt-x",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let added = chat_completions_catalog().models()[0]
        .wire_protocol
        .enforce_usage_reporting(&mut body);

    assert_eq!(added, vec!["stream_options"]);
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
}

/// Why the dialect had to travel on the quote rather than be looked up.
///
/// `Target::Frontier` keys on provider and model only, so a catalog holding one
/// model over two dialects — OpenAI serves `gpt-x` on both Chat Completions and
/// Responses — cannot be disambiguated by target. `spec_for` would silently
/// return the first. That is why `CatalogConfig` refuses a duplicated identity
/// outright: the boundary check is what makes the lookup in the engine a lookup
/// rather than a coin flip, and this test is the reason it cannot be relaxed.
#[test]
fn target_alone_does_not_identify_a_dialect() {
    let spec = |wire_protocol| FrontierModelSpec {
        wire_protocol,
        ..chat_completions_catalog().models()[0].clone()
    };
    let catalog = StaticFrontierCatalog::new(vec![
        spec(WireProtocol::OpenAiChatCompletions),
        spec(WireProtocol::OpenAiResponses),
    ]);

    let mut ledger = CacheLedger::new();
    catalog.apply_to_ledger(&mut ledger);
    let candidates = catalog.quote(&ledger, 0, 1_000, 500);
    let chosen = &candidates[1].target;

    let matching: Vec<_> = catalog
        .models()
        .iter()
        .filter(|spec| &spec.target() == chosen)
        .map(|spec| spec.wire_protocol)
        .collect();
    assert_eq!(
        matching,
        vec![
            WireProtocol::OpenAiChatCompletions,
            WireProtocol::OpenAiResponses
        ],
        "two dialects share one target, so a catalog lookup keyed on the \
         target cannot tell a client which one it was asked to speak",
    );
}
