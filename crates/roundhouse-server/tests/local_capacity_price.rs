// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A configured price for local capacity (ruled 2026-09-28, ruling 6 in
//! `agent-docs/synergies/typesafe-selector-and-cache-affinity.md`).
//!
//! Until this change a local candidate quoted zero dollars on every turn, so
//! local won every cost comparison against a hosted target and the router gave
//! no signal of cost effectiveness. The claims here are the join and its
//! consequence: a price written in the catalog file reaches the candidate the
//! router compares, and with it a local worker can lose on cost to a cheaper
//! hosted target, which it never could at zero.
//!
//! Driven through [`Engine::run_turn`] with a real selection service behind
//! it, for the reason `local_ttft_config.rs` gives: the join breaks either in
//! the loader or at the engine's call site, and a hand-built `EngineConfig`
//! would skip the first.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::policy::Weights;
use roundhouse_core::routing::{AffinityPolicy, Candidate, DecisionRecord};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierClient, FrontierClients, LocalFleet, WireProtocol,
};
use roundhouse_server::catalog_config::engine_config;
use roundhouse_server::{
    Admission, CatalogConfig, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor, TurnInput,
};

mod common;

/// A catalog with one hosted model and whatever local section the test sets.
///
/// Through [`CatalogConfig::from_json`] so the serde shape an operator writes
/// is what is under test.
fn catalog(local_section: &str) -> CatalogConfig {
    let json = format!(
        r#"{{
          "models": [{{
            "provider": "anthropic",
            "model": "claude",
            "wire_protocol": "anthropic_messages",
            "cache_model": {{ "kind": "deterministic", "ttl_ms": 300000 }},
            "pricing": {{
              "input_per_mtok_usd": 3.0,
              "cached_input_per_mtok_usd": 0.3,
              "cache_write_per_mtok_usd": 3.75,
              "output_per_mtok_usd": 15.0
            }},
            "quality_prior": 0.62,
            "base_ttft_ms": 350.0,
            "ttft_ms_per_uncached_token": 0.002
          }}],
          "providers": {{
            "anthropic": {{
              "base_url": "https://api.anthropic.test/v1",
              "routes": {{ "messages": "/messages" }},
              "auth": {{ "env": "ANTHROPIC_API_KEY" }}
            }}
          }}{local_section}
        }}"#
    );
    CatalogConfig::from_json(&json, "test").expect("the fixture catalog validates")
}

/// A local price well above the hosted rate card above, on both axes.
const DEAR_LOCAL: &str = r#",
  "local_capacity_price": { "input_per_mtok_usd": 30.0, "output_per_mtok_usd": 150.0 }"#;

/// Everything one turn routed on: the decision, from the log.
///
/// The router weighs cost alone. Prefill and TTFT are normalized over the
/// pool, so a one-token difference between the local and hosted prefill
/// estimates would swing a whole weight unit either way and decide the turn
/// for a reason this test is not about. With the other two axes at zero the
/// only thing that can move the decision is the cost the catalog set.
async fn routed(config: &CatalogConfig) -> DecisionRecord {
    let store = Arc::new(MemoryStore::new());
    let clients = FrontierClients::keyed(
        [(
            "anthropic".to_string(),
            Arc::new(EchoFrontierClient::new("frontier answer")) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let engine = Engine::with_provider_clients(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        config.catalog(),
        Arc::new(clients),
        Arc::new(AffinityPolicy::new().with_weights(Weights {
            prefill: 0.0,
            cost: 1.0,
            ttft: 0.0,
        })),
        EngineConfig {
            block_size: common::BLOCK_SIZE,
            local_model: common::LOCAL_MODEL.to_string(),
            turn_deadline_ms: 5_000,
            ..engine_config(Some(config))
        },
    )
    .with_fleet(common::embedded_fleet().await as Arc<dyn LocalFleet>);

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
                items: vec![Item::user_text("cache affinity ".repeat(200))],
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

    store
        .read_events(&session_id, 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .find_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .expect("the turn routed")
}

fn local_candidate(decision: &DecisionRecord) -> &Candidate {
    let candidate = decision
        .considered
        .iter()
        .find(|candidate| candidate.target.is_local())
        .expect("a turn with no toolbox is priced against the local fleet");
    assert!(
        candidate.expected_prefill_tokens > 0.0,
        "the fixture must give the worker something to prefill, or the input \
         rate multiplies zero and the price assertions prove less"
    );
    candidate
}

fn hosted_candidate(decision: &DecisionRecord) -> &Candidate {
    decision
        .considered
        .iter()
        .find(|candidate| !candidate.target.is_local())
        .expect("the catalog's hosted model is quoted")
}

/// The file's price is the price the router quotes: uncached prefill at the
/// input rate plus the engine's expected output at the output rate.
#[tokio::test]
async fn the_catalogs_capacity_price_reaches_the_local_quote() {
    let config = catalog(DEAR_LOCAL);

    let decision = routed(&config).await;
    let local = local_candidate(&decision);

    let expected_output = f64::from(EngineConfig::default().expected_output_tokens);
    let expected = local.expected_prefill_tokens * 30.0 / 1e6 + expected_output * 150.0 / 1e6;
    assert!(
        (local.expected_cost_usd - expected).abs() < 1e-12,
        "the local quote must be prefill at the input rate plus expected output \
         at the output rate: got {}, want {expected}",
        local.expected_cost_usd
    );
}

/// **A priced local worker loses on cost to a cheaper hosted target.**
#[tokio::test]
async fn a_priced_local_worker_loses_to_a_cheaper_hosted_target() {
    let decision = routed(&catalog(DEAR_LOCAL)).await;

    assert!(
        local_candidate(&decision).expected_cost_usd
            > hosted_candidate(&decision).expected_cost_usd,
        "the fixture must price local above the hosted quote, or the decision \
         below is not about cost"
    );
    assert!(
        !decision.chosen.is_local(),
        "a local worker dearer than the hosted target must lose on cost: {}",
        decision.rationale
    );
}

/// **CONTROL.** The same turn with no price configured: local quotes zero,
/// and on cost alone it wins. This is what the test above could never see
/// before the price existed, and what proves the test above is about the price.
#[tokio::test]
async fn an_unpriced_local_worker_quotes_zero_and_wins_on_cost() {
    let decision = routed(&catalog("")).await;

    assert_eq!(local_candidate(&decision).expected_cost_usd, 0.0);
    assert!(
        decision.chosen.is_local(),
        "a zero-dollar local quote wins every cost comparison: {}",
        decision.rationale
    );
}
