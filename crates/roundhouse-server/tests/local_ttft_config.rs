// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! C5 of `PLAN-cache-affinity.md` — a deployment sets the local TTFT curve.
//!
//! `LocalQuote::to_candidate` has taken a base and a slope since the mechanism
//! landed, and its own unit tests prove the arithmetic. They cannot prove the
//! rung: until a loader set the slope, every deployment ran the built-in
//! `0.0` and quoted a local worker flat no matter how much prefill the
//! residency answer reported. So the claim here is the join — a number written
//! in the catalog file reaches the candidate the router compares — and it is
//! asked through [`Engine::run_turn`] with a real selection service behind it,
//! because the two places the join can break are the loader and the engine's
//! own call site, and a test that built the `EngineConfig` by hand would skip
//! the first.
//!
//! Read the local candidate out of `considered` rather than asserting which
//! target won: the quote is an input to the decision and this rung does not
//! change what the decision does with it.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
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

/// A catalog carrying `local`, and whatever local section the test is about.
///
/// Through [`CatalogConfig::from_json`] in every test rather than through a
/// hand-built struct, because the file *is* the format: the serde defaults are
/// half of what this rung ships, and a fixture that assigned the fields
/// directly would prove nothing about the catalog an operator writes.
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

/// The engine a deployment holding `config` would compose.
///
/// The three fields overridden after [`engine_config`] are the fixture's own —
/// which model the registered worker serves, its block size, and a deadline
/// short enough to fail rather than hang. Everything the rung is about arrives
/// through the spread, so a loader that dropped a field shows up here as a
/// wrong quote rather than as a compile error.
async fn engine(
    config: &CatalogConfig,
) -> (Arc<Engine<MemoryStore, ByteTokenizer>>, Arc<MemoryStore>) {
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
        common::frontier_catalog(),
        Arc::new(clients),
        Arc::new(AffinityPolicy::new()),
        EngineConfig {
            block_size: common::BLOCK_SIZE,
            local_model: common::LOCAL_MODEL.to_string(),
            turn_deadline_ms: 5_000,
            ..engine_config(Some(config))
        },
    )
    .with_fleet(common::embedded_fleet().await as Arc<dyn LocalFleet>);
    (Arc::new(engine), store)
}

/// The local candidate the router compared, from one turn with no toolbox.
///
/// A prompt long enough that the selection service reports prefill to do: at
/// zero effective prefill tokens every slope quotes the same number, and the
/// assertions below would hold for a loader that dropped the field entirely.
async fn local_candidate(config: &CatalogConfig) -> Candidate {
    let (engine, store) = engine(config).await;
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
                items: vec![Item::user_text(&"cache affinity ".repeat(200))],
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

    let decision: DecisionRecord = store
        .read_events(&session_id, 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .find_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .expect("the turn routed");

    let candidate = decision
        .considered
        .into_iter()
        .find(|candidate| candidate.target.is_local())
        .expect("a turn with no toolbox is priced against the local fleet");
    assert!(
        candidate.expected_prefill_tokens > 0.0,
        "the fixture must give the worker something to prefill, or every slope \
         quotes the same number and the assertions below prove nothing"
    );
    candidate
}

/// The measured slope in the file is the slope the router quotes on.
#[tokio::test]
async fn a_measured_slope_in_the_catalog_reaches_the_local_quote() {
    let config = catalog(
        r#",
          "local_base_ttft_ms": 90.0,
          "local_ttft_ms_per_prefill_token": 0.25"#,
    );

    let candidate = local_candidate(&config).await;

    assert_eq!(
        candidate.expected_ttft_ms,
        90.0 + candidate.expected_prefill_tokens * 0.25,
        "the quote must be the file's floor plus the file's slope over the \
         residency answer's own prefill tokens"
    );
    assert!(
        candidate.expected_ttft_ms > 90.0,
        "a cold local target must carry a time penalty, not just a floor"
    );
}

/// **CONTROL.** A catalog that measured nothing quotes exactly what it did
/// before this rung.
///
/// Both halves matter: the flat quote is the documented default, and it is
/// also what a loader that silently zeroed the configured slope would produce,
/// so this passing alone is not evidence the wiring works — the test above is.
#[tokio::test]
async fn a_catalog_that_sets_no_local_curve_quotes_the_flat_base() {
    let config = catalog("");

    let candidate = local_candidate(&config).await;

    assert_eq!(
        candidate.expected_ttft_ms,
        EngineConfig::default().local_base_ttft_ms,
        "an unmeasured deployment must quote the built-in floor flat, however \
         much prefill the residency answer reports"
    );
}
