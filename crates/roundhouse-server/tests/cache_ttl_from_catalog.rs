// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! C4 of `PLAN-cache-affinity.md` — the catalog's TTL reaches the request.
//!
//! One lifetime per target, taken from `FrontierModelSpec::cache_model` and
//! carried on the quote, so the ledger's retention prediction and the marker on
//! the wire cannot name different numbers.
//!
//! This binary owns the engine half of that join: what `connect` puts on the
//! `FrontierQuote` for a chosen target. The wire half — what
//! `AnthropicMessagesClient::body` serializes from the field — is asserted in
//! `roundhouse-fleet`, where `body` is reachable.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, CacheModel};
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::anthropic_messages::CacheLifetime;
use roundhouse_fleet::{
    EchoFrontierClient, FrontierClient, FrontierClients, FrontierError, FrontierQuote,
    FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, EngineError, LocalExecutor, TurnInput,
};

/// A client that keeps the quote it was handed and then answers normally.
///
/// Wrapping [`EchoFrontierClient`] rather than replacing it: the turn has to
/// complete for the engine to have built a quote at all, and a double that
/// returned nothing would fail the turn before the assertion.
struct RecordingClient {
    inner: EchoFrontierClient,
    seen: Mutex<Vec<FrontierQuote>>,
}

#[async_trait]
impl FrontierClient for RecordingClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.seen
            .lock()
            .expect("no panic holds this")
            .push(quote.clone());
        self.inner.execute(quote).await
    }
}

/// The quote the engine built for a catalog whose one entry has `cache_model`.
async fn dispatched_quote(cache_model: CacheModel, wire_protocol: WireProtocol) -> FrontierQuote {
    let mut spec = frontier_spec("anthropic", "claude", wire_protocol);
    spec.cache_model = cache_model;
    // An hour-long entry has to carry the hour's write rate or the catalog
    // boundary refuses it; this fixture is built by hand rather than parsed,
    // so it honours the same rule to stay a legal deployment.
    spec.pricing.cache_write_per_mtok_usd = 2.0 * spec.pricing.input_per_mtok_usd;
    let recorder = Arc::new(RecordingClient {
        inner: EchoFrontierClient::new("frontier answer"),
        seen: Mutex::new(Vec::new()),
    });
    let clients = FrontierClients::keyed(
        [(
            "anthropic".to_string(),
            Arc::clone(&recorder) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let engine = Engine::with_provider_clients(
        Arc::new(MemoryStore::new()),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        StaticFrontierCatalog::new(vec![spec]),
        Arc::new(clients),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    );

    let session_id = SessionId::generate();
    engine
        .create_session(&session_id)
        .await
        .expect("the session opens");
    engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            TurnInput {
                items: vec![Item::user_text("hi")],
                declared_baseline: None,
                output_token_cap: None,
                tools: None,
                tool_choice: None,
                tools_dialect: Some(WireProtocol::AnthropicMessages),
            },
            &Admission::open(),
        )
        .await
        .expect("the turn is served");

    let seen = recorder.seen.lock().expect("no panic holds this");
    assert_eq!(seen.len(), 1, "the turn dispatched exactly once");
    seen[0].clone()
}

/// A declared hour reaches the quote as an hour.
#[tokio::test]
async fn a_one_hour_catalog_entry_puts_an_hour_on_the_quote() {
    let quote = dispatched_quote(
        CacheModel::Deterministic { ttl_ms: 3_600_000 },
        WireProtocol::AnthropicMessages,
    )
    .await;

    assert_eq!(quote.cache_lifetime, CacheLifetime::OneHour);
}

/// **CONTROL.** The five-minute entry every catalog ships today.
#[tokio::test]
async fn a_five_minute_catalog_entry_puts_five_minutes_on_the_quote() {
    let quote = dispatched_quote(
        CacheModel::Deterministic { ttl_ms: 300_000 },
        WireProtocol::AnthropicMessages,
    )
    .await;

    assert_eq!(
        quote.cache_lifetime,
        CacheLifetime::Default,
        "the ledger predicts retention on this number, so the request must ask \
         for the same one"
    );
}

/// **CONTROL.** A cache with no fixed lifetime asks for none.
///
/// An automatic cache expires on its own schedule, and naming a lifetime the
/// provider never promised would put a number on the wire that the ledger's
/// own decay model contradicts.
#[tokio::test]
async fn an_automatic_cache_model_puts_no_lifetime_on_the_quote() {
    let quote = dispatched_quote(
        CacheModel::InactivityDecay {
            half_life_ms: 300_000,
            max_ttl_ms: 3_600_000,
            min_prefix_tokens: 1024,
        },
        WireProtocol::OpenAiResponses,
    )
    .await;

    assert_eq!(quote.cache_lifetime, CacheLifetime::Default);
}

/// **CORRECTNESS (fleet-redis-2, dispatch-time).** A spec that skipped
/// `CatalogConfig`'s own boot refusal of an unspellable Messages TTL still
/// must not reach a socket -- `connect` resolves the lifetime before it
/// builds the quote, and a turn that cannot resolve one is refused, not
/// dispatched at the wire's own five-minute default.
#[tokio::test]
async fn an_unspellable_messages_ttl_dispatches_nothing_and_fails_the_turn() {
    let mut spec = frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages);
    spec.cache_model = CacheModel::Deterministic { ttl_ms: 600_000 };
    let recorder = Arc::new(RecordingClient {
        inner: EchoFrontierClient::new("frontier answer"),
        seen: Mutex::new(Vec::new()),
    });
    let clients = FrontierClients::keyed(
        [(
            "anthropic".to_string(),
            Arc::clone(&recorder) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let engine = Engine::with_provider_clients(
        Arc::new(MemoryStore::new()),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        StaticFrontierCatalog::new(vec![spec]),
        Arc::new(clients),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            turn_deadline_ms: 5_000,
            ..EngineConfig::default()
        },
    );

    let session_id = SessionId::generate();
    engine
        .create_session(&session_id)
        .await
        .expect("the session opens");
    let error = engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            TurnInput {
                items: vec![Item::user_text("hi")],
                declared_baseline: None,
                output_token_cap: None,
                tools: None,
                tool_choice: None,
                tools_dialect: Some(WireProtocol::AnthropicMessages),
            },
            &Admission::open(),
        )
        .await
        .expect_err("an unspellable cache lifetime must fail the turn rather than dispatch it");

    assert!(
        matches!(
            &error,
            EngineError::Frontier(FrontierError::UnsupportedCacheLifetime {
                ttl_ms: 600_000,
                ..
            })
        ),
        "expected the resolver's own refusal, got {error}"
    );
    assert!(
        recorder
            .seen
            .lock()
            .expect("no panic holds this")
            .is_empty(),
        "the client must never see a request built from an unresolved lifetime"
    );
}
