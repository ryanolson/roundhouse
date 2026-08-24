// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M10.1 P2: a turn is dispatched through *its own provider's* transport.
//!
//! The unit tests one layer down prove that `FrontierClients::for_provider`
//! returns what was put in it, and the boot tests in `main.rs` prove the
//! registry refuses a provider it cannot serve. What neither can prove is the
//! thing the phase actually needs: that the engine resolves the client from the
//! chosen target's catalog entry rather than from a field it was handed once at
//! composition.
//!
//! The difference is invisible in every deployment that addresses one origin,
//! which is why it survived until now — and it is exactly wrong for the session
//! this phase exists to serve, where one turn's candidate list spans OpenRouter
//! and OpenAI's own endpoint. A registry that resolved to the wrong client would
//! authenticate one provider's key against another's base URL: an outage that
//! reads as a credential problem, on a route nobody configured.
//!
//! So every claim here is about *which of two recorders was called*, and each
//! has a control that varies only the registry — because an assertion that one
//! recorder saw the turn would also pass on an engine that had exactly one
//! client.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing, Target};
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::{
    EchoFrontierClient, FrontierChunk, FrontierClient, FrontierClients, FrontierError,
    FrontierModelSpec, FrontierQuote, FrontierStream, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::{Admission, EchoLocalExecutor, Engine, LocalExecutor, TurnResult};

mod common;
use common::{MINUTE, config};

/// A transport that answers with its own name and counts what it was asked to
/// serve.
///
/// The name is in the *answer* as well as in the counter on purpose: a turn's
/// text is what a client sees, so a test that read only the counter could not
/// tell "the right client was called" from "the right client was called and
/// something else produced the answer".
struct Recorder {
    name: &'static str,
    calls: AtomicUsize,
}

impl Recorder {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl FrontierClient for Recorder {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(FrontierChunk::whole_response(
            self.name.to_string(),
            quote.prompt.len() as u64,
            0,
            self.name.len() as u64,
            0,
        ))
    }
}

/// One hosted entry, free and instant, so the router has exactly one thing it
/// could pick and the assertion is never about scoring.
fn catalog_for(provider: &str) -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
        provider: provider.into(),
        model: "flagship".into(),
        wire_protocol: WireProtocol::OpenAiResponses,
        cache_model: CacheModel::Deterministic { ttl_ms: 5 * MINUTE },
        pricing: ProviderPricing::free(),
        quality_prior: 0.9,
        base_ttft_ms: 1.0,
        ttft_ms_per_uncached_token: 0.0,
    }])
}

fn engine_over(
    catalog: StaticFrontierCatalog,
    clients: FrontierClients,
) -> Engine<MemoryStore, ByteTokenizer> {
    Engine::with_provider_clients(
        Arc::new(MemoryStore::new()),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        catalog,
        Arc::new(clients),
        Arc::new(AffinityPolicy::new()),
        config(),
    )
}

/// Run one turn against a catalog whose only entry belongs to `provider`.
///
/// The engine wires no `LocalFleet`, so the hosted entry is the only candidate
/// and the routing decision is fixed before any of this is about the registry.
async fn one_turn(engine: &Engine<MemoryStore, ByteTokenizer>) -> TurnResult {
    let session_id = SessionId::generate();
    engine
        .create_session(&session_id)
        .await
        .expect("the session store is in memory");
    engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text("which client served this?")],
            &Admission::open(),
        )
        .await
        .expect("one hosted candidate, one transport, one turn")
}

/// **The claim.** Two providers, two transports, and the entry decides.
#[tokio::test]
async fn a_catalog_entry_dispatches_through_its_own_providers_client() {
    let alpha = Recorder::new("alpha-served-this");
    let beta = Recorder::new("beta-served-this");
    let registry = || {
        FrontierClients::keyed(
            [
                (
                    "alpha".to_string(),
                    Arc::clone(&alpha) as Arc<dyn FrontierClient>,
                ),
                (
                    "beta".to_string(),
                    Arc::clone(&beta) as Arc<dyn FrontierClient>,
                ),
            ]
            .into_iter()
            .collect(),
        )
    };

    let result = one_turn(&engine_over(catalog_for("alpha"), registry())).await;
    assert_eq!(result.text, "alpha-served-this");
    assert_eq!((alpha.calls(), beta.calls()), (1, 0));

    // The identical registry, the identical engine shape, one catalog entry
    // different. If the engine held a client rather than resolving one, this is
    // the assertion that would go red — and it is the whole difference between
    // M10.0's single transport and M10.1's.
    let result = one_turn(&engine_over(catalog_for("beta"), registry())).await;
    assert_eq!(result.text, "beta-served-this");
    assert_eq!((alpha.calls(), beta.calls()), (1, 1));
}

/// The control that stops the test above passing for the wrong reason.
///
/// A *uniform* registry answers for every provider name with one transport,
/// which is what every pre-M10.1 deployment and every echo-stub test is. Under
/// it the entry's provider changes nothing — so the pair together say the
/// answer moves with the catalog entry only when the registry is keyed, rather
/// than that these two recorders happen to differ.
#[tokio::test]
async fn control_a_uniform_registry_serves_every_provider_from_one_transport() {
    let only = Recorder::new("the-only-transport");

    for provider in ["alpha", "beta", "a-provider-nobody-mentioned"] {
        let engine = engine_over(
            catalog_for(provider),
            FrontierClients::uniform(Arc::clone(&only) as Arc<dyn FrontierClient>),
        );
        assert_eq!(one_turn(&engine).await.text, "the-only-transport");
    }
    assert_eq!(only.calls(), 3);
}

/// A keyed registry has no fallback, and the turn fails rather than landing
/// somewhere nobody configured.
///
/// Unreachable through configuration on a booted deployment — the catalog
/// boundary and the registry constructor between them refuse a provider with no
/// transport before a session exists — which is precisely why it is asserted
/// here: a hand-built catalog carries that obligation itself, and the answer to
/// breaking it must be a failed turn rather than a turn served by whichever
/// client the map iterated first.
#[tokio::test]
async fn a_provider_the_registry_does_not_hold_fails_the_turn_rather_than_borrowing_a_client() {
    let engine = engine_over(
        catalog_for("undefined"),
        FrontierClients::keyed(
            [(
                "alpha".to_string(),
                Arc::new(EchoFrontierClient::new("alpha")) as Arc<dyn FrontierClient>,
            )]
            .into_iter()
            .collect(),
        ),
    );

    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();
    let error = engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text("hi")],
            &Admission::open(),
        )
        .await
        .expect_err("a provider with no transport must not borrow another's");
    let message = error.to_string();
    assert!(
        message.contains("undefined"),
        "the failure has to name the provider, because the remedy is a definition: {message}"
    );

    // CONTROL: the same registry serving the target it does hold. One catalog
    // entry different, and the turn completes — which is what makes the failure
    // above about the missing key rather than about `keyed` registries failing
    // every turn.
    let engine = engine_over(
        catalog_for("alpha"),
        FrontierClients::keyed(
            [(
                "alpha".to_string(),
                Arc::new(EchoFrontierClient::new("alpha")) as Arc<dyn FrontierClient>,
            )]
            .into_iter()
            .collect(),
        ),
    );
    assert_eq!(one_turn(&engine).await.text, "alpha");
}

/// The routing decision the tests above lean on: the hosted entry is what the
/// turn chose, so "which client was called" is a statement about the registry
/// and not about a local fallback quietly serving everything.
#[tokio::test]
async fn control_the_turn_really_routed_to_the_hosted_entry() {
    let engine = engine_over(
        catalog_for("alpha"),
        FrontierClients::uniform(Recorder::new("alpha") as Arc<dyn FrontierClient>),
    );
    let decision = one_turn(&engine)
        .await
        .decision
        .expect("a dispatched turn records its decision");
    assert_eq!(
        decision.target,
        Target::Frontier {
            provider: "alpha".into(),
            model: "flagship".into(),
        }
    );
}
