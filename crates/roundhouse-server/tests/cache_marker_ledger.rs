// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The next quote is warm only through the last cache marker the previous
//! request actually carried.
//!
//! This binary owns the engine half of the join: that a dispatch records
//! where its own marker went on its `Routed` decision, and that the following
//! turn's quote reads that record. The placement rule itself, and that the
//! recorded placement is the one the body carries, are asserted in
//! `roundhouse-fleet`, where `body` is reachable.

use std::sync::Arc;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::{AffinityPolicy, BlockMarker, CacheModel, DecisionRecord};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};
use roundhouse_server::test_support::frontier_spec;
use roundhouse_server::{
    Admission, EchoLocalExecutor, Engine, EngineConfig, LocalExecutor, TurnInput,
};

/// The stable first item, long enough that the prefix it caches is most of
/// the first request and the unmarked final item is still worth pricing.
const FIRST: &str = "a stable system preamble that every later turn resends byte for byte";
/// The first turn's final item: sent, never marked, so never cached.
const SECOND: &str = "the first question, which only the next turn's marker could cache";

/// Two turns against a one-entry catalog, and the `Routed` decision of each.
async fn two_turns(spec: FrontierModelSpec) -> (DecisionRecord, DecisionRecord) {
    two_turns_declaring(spec, None).await.0
}

/// As [`two_turns`], with `tools` declared on the first turn, and the engine's
/// own token count of that toolbox.
async fn two_turns_declaring(
    spec: FrontierModelSpec,
    tools: Option<serde_json::Value>,
) -> ((DecisionRecord, DecisionRecord), u64) {
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(
        Arc::clone(&store),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")) as Arc<dyn LocalExecutor>,
        StaticFrontierCatalog::new(vec![spec]),
        Arc::new(EchoFrontierClient::new("frontier answer")),
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
    let toolbox_tokens = engine.declaration_tokens(tools.as_ref(), None);
    let mut first_turn: TurnInput = vec![Item::user_text(FIRST), Item::user_text(SECOND)].into();
    if tools.is_some() {
        first_turn.tools = tools;
        first_turn.tools_dialect = Some(WireProtocol::AnthropicMessages);
    }
    engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            first_turn,
            &Admission::open(),
        )
        .await
        .expect("the first turn is served");
    engine
        .run_turn(
            &session_id,
            TurnId::new("t2"),
            vec![Item::user_text("a follow-up")],
            &Admission::open(),
        )
        .await
        .expect("the second turn is served");

    let mut decisions: Vec<DecisionRecord> = store
        .read_events(&session_id, 0, 1_000)
        .await
        .expect("an in-memory log reads")
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::Routed { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.len(), 2, "one dispatch per turn: {decisions:?}");
    let second = decisions.pop().expect("two decisions");
    let first = decisions.pop().expect("two decisions");
    ((first, second), toolbox_tokens)
}

/// **P2, end to end.** The first Anthropic dispatch marks block 0 (two items,
/// penultimate marker) and records it; the second turn's quote then prices
/// only that block as a read and the first turn's final item as uncached
/// input — the write the provider bills it as.
#[tokio::test]
async fn the_second_anthropic_quote_prices_the_previous_final_item_as_uncached() {
    let spec = frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages);
    let pricing = spec.pricing;
    let (first, second) = two_turns(spec).await;

    // Counted from the renders, independently of the ledger: the byte
    // tokenizer's token count of an item is its render's length.
    let marked_prefix = Item::user_text(FIRST).render().len() as u64;
    let final_item = Item::user_text(SECOND).render().len() as u64;
    assert_eq!(first.isl_tokens, marked_prefix + final_item);
    assert_eq!(
        first.block_marker,
        Some(BlockMarker::Placed {
            segment: 0,
            prefix_tokens: marked_prefix,
        }),
        "two items: the penultimate one is marked, and nothing else"
    );

    let output = EngineConfig::default().expected_output_tokens as f64;
    let isl = second.isl_tokens as f64;
    let expected = pricing.price_tokens(isl - marked_prefix as f64, marked_prefix as f64, output);
    assert!(
        (second.expected_cost_usd - expected).abs() < 1e-12,
        "the second quote must read only the marked {marked_prefix} tokens and pay the \
         first turn's final {final_item} as uncached input: quoted {}, expected {expected}",
        second.expected_cost_usd,
    );
    // And that is dearer than the whole-prompt prediction it replaces, by the
    // final item's write-over-read premium.
    let whole = first.isl_tokens as f64;
    let flattering = pricing.price_tokens(isl - whole, whole, output);
    assert!(second.expected_cost_usd > flattering);
}

/// **CONTROL.** A Responses target caches without markers, records no
/// placement, and keeps the whole previous prompt as its warm prefix.
#[tokio::test]
async fn a_responses_target_records_no_marker_and_keeps_the_whole_prompt_prediction() {
    let mut spec = frontier_spec("openai", "gpt", WireProtocol::OpenAiResponses);
    // A decay slow enough that the milliseconds between the two turns leave
    // the hit probability indistinguishable from one.
    spec.cache_model = CacheModel::InactivityDecay {
        half_life_ms: 3_600_000,
        max_ttl_ms: 3_600_000,
        min_prefix_tokens: 1,
    };
    let pricing = spec.pricing;
    let (first, second) = two_turns(spec).await;

    assert_eq!(first.block_marker, None);
    let output = EngineConfig::default().expected_output_tokens as f64;
    let isl = second.isl_tokens as f64;
    let whole = first.isl_tokens as f64;
    let expected = pricing.price_tokens(isl - whole, whole, output);
    assert!(
        (second.expected_cost_usd - expected).abs() < 1e-9,
        "quoted {}, expected {expected}",
        second.expected_cost_usd,
    );
}

/// **The toolbox is part of the prefix a block marker caches.** Anthropic
/// renders tools ahead of the messages, so a marker on block 0 caches the tool
/// declarations and that block together. A recorded prefix without the
/// toolbox would under-state the warm prefix of every tool-declaring turn.
#[tokio::test]
async fn a_marked_prefix_includes_the_declared_toolbox() {
    let spec = frontier_spec("anthropic", "claude", WireProtocol::AnthropicMessages);
    let tools = serde_json::json!([{
        "name": "read_file",
        "description": "Read a file from the workspace",
        "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } }
    }]);
    let ((first, _), toolbox_tokens) = two_turns_declaring(spec, Some(tools)).await;

    assert!(
        toolbox_tokens > 0,
        "the premise: the toolbox has a token count"
    );
    let marked_block = Item::user_text(FIRST).render().len() as u64;
    assert_eq!(
        first.block_marker,
        Some(BlockMarker::Placed {
            segment: 0,
            prefix_tokens: toolbox_tokens + marked_block,
        }),
        "the recorded prefix is the toolbox plus the marked block"
    );
}
