// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M17 thermo-nuclear review, finding F7 — confirmed.
//!
//! **The claim.** `engine.rs:1621`'s `Item::namespaced_tool_call` stores
//! whatever namespace `FrontierChunk::ToolCall` carries, with no read of
//! which client surface is serving the session. If a namespace ever reaches
//! that join on a session served over `/v1/messages`, the finding says the
//! session forks on every subsequent turn: the Messages wire canonicalizes a
//! resent `tool_use` with `namespace: None` (`messages_api/wire.rs`'s
//! `block_item`), `same_namespace(Some(_), None)` in `prefix_admission.rs` is
//! `false`, and prefix admission finds no matching prefix and forks to a new
//! generation.
//!
//! **This test.** [`a_namespaced_tool_call_on_the_messages_surface_does_not_fork`]
//! builds a real `Engine` + `messages_router` behind a small local
//! `FrontierClient` double — [`NamespacedThenText`] — that answers the first
//! turn with `FrontierChunk::ToolCall { namespace: Some("mcp__roundhouse"),
//! .. }`, the exact shape `ToolCallingFrontierClient` in
//! `tests/common/mod.rs` cannot produce (it hard-codes `namespace: None`).
//! It drives one real turn over `/v1/messages`, confirms the stored item
//! carries no namespace (the invariant the fix now enforces at the join,
//! and the premise the fork claim depends on *not* holding), accumulates
//! the stream with the strict Messages oracle in `common::anthropic`,
//! resends the accumulated `tool_use` block plus a `tool_result` exactly as
//! Claude Code's own accumulator would (the Messages wire has no
//! `namespace` key on a `tool_use` block to resend, so the resent block
//! necessarily claims `namespace: None`), and checks that the session did
//! not fork to a `#g1` generation.
//!
//! **Failed before the fix, for exactly the mechanism the finding names.**
//! `engine.rs`'s namespace join stored whatever `FrontierChunk::ToolCall`
//! carried with no read of which surface served the session, so the first
//! turn's committed item carried `Some("mcp__roundhouse")` on a
//! Messages-surface session. The second turn then opened
//! `anthropic_messages/f7-namespace-messages#g1`:
//! `same_namespace(Some("mcp__roundhouse"), None)` in `prefix_admission.rs`
//! evaluates its `Some(_) => stored == claimed` arm, which is `false`
//! (`ItemContent::ToolCall`'s `namespace` field differs), so `same_item`
//! rejects the pairing, `suffix_after` finds no matching prefix, and
//! `bind_prefix` forks. The name-and-arguments agreement that would
//! otherwise admit the resend as a plain continuation was irrelevant once
//! the namespace arm returned `false`: `same_item`'s `ItemContent::ToolCall`
//! match arm returns the whole boolean from that one field's comparison,
//! not a conjunction that could still pass on the rest.
//!
//! Ruling: **valid**. The mechanism in the finding is exactly what fired;
//! nothing about it needed correcting. Fixed at the producer named in the
//! finding: the join now reads `ControlCallDialect::of_session_key` (the
//! same split `run_turn`'s admission and `plan`'s `TurnSignals` already use)
//! and stores the decoded namespace only for `CodexResponses`, `None` for
//! `ClaudeMessages` — the dialect that has no wire field to ever resend a
//! `Some(_)` through. One caveat still worth naming: this test does not
//! establish that an upstream *can* return a namespace on a session
//! actually routed to a Messages-declared toolbox in production today (the
//! finding's own `frontier.rs:678-711` caveat) — only that *if* one does,
//! the join now discards it before it can fork anything.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
};
use roundhouse_server::conversations::bound_session;
use roundhouse_server::test_support::engine_over_echo;
use roundhouse_server::{ControlPlane, Conversations, messages_router};

mod common;
use common::anthropic::audit;
use common::{config, frontier_catalog};

/// The namespace an upstream names an MCP call under, per R-N6 — the same
/// literal `review_m17_f3.rs` and `steering_emission.rs` use.
const NAMESPACE: &str = "mcp__roundhouse";

/// A `FrontierClient` that answers the first turn with one namespaced tool
/// call and every later turn with a plain text answer — the queue-then-tail
/// shape `ScriptedTurns` uses, needed here because `ScriptedTurns` is built on
/// `ToolCallingFrontierClient`, which cannot emit a namespace at all.
struct NamespacedThenText {
    call_id: &'static str,
    tool_name: &'static str,
    arguments: &'static str,
    answered: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl FrontierClient for NamespacedThenText {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        if self
            .answered
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let chunks: Vec<Result<FrontierChunk, FrontierError>> = vec![
                Ok(FrontierChunk::OutputText("done".to_string())),
                Ok(FrontierChunk::Done {
                    input_tokens: quote.prompt.len() as u64,
                    cached_input_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 4,
                    reasoning_tokens: 0,
                    provider_reported_cost: None,
                    stop_reason: Some("end_turn".to_string()),
                }),
            ];
            return Ok(futures::stream::iter(chunks).boxed());
        }
        let chunks: Vec<Result<FrontierChunk, FrontierError>> = vec![
            Ok(FrontierChunk::ToolCall {
                id: self.call_id.to_string(),
                name: self.tool_name.to_string(),
                namespace: Some(NAMESPACE.to_string()),
                arguments: self.arguments.to_string(),
            }),
            Ok(FrontierChunk::Done {
                input_tokens: quote.prompt.len() as u64,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: self.arguments.len() as u64,
                reasoning_tokens: 0,
                provider_reported_cost: None,
                stop_reason: Some("tool_use".to_string()),
            }),
        ];
        Ok(futures::stream::iter(chunks).boxed())
    }
}

/// The Messages surface over the double above.
fn deployment() -> (Router, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(NamespacedThenText {
        call_id: "toolu_f7",
        tool_name: "Grep",
        arguments: r#"{"pattern": "fn main", "path": "/src"}"#,
        answered: std::sync::atomic::AtomicBool::new(false),
    });
    let engine = Arc::new(engine_over_echo(
        Arc::clone(&store),
        frontier_catalog(),
        client as Arc<dyn FrontierClient>,
        config(),
    ));
    let app = messages_router(
        ControlPlane::open(),
        engine,
        Arc::clone(&store),
        Arc::new(Conversations::new()),
    );
    (app, store)
}

async fn post(app: &Router, headers: &[(&str, &str)], body: &serde_json::Value) -> String {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(body).expect("json")))
                .expect("request"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a readable body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("bodies are UTF-8")
}

fn named(session: &str) -> String {
    format!("anthropic_messages/{session}")
}

async fn no_such_session(store: &MemoryStore, session_id: &str) -> bool {
    store
        .last_seq(&roundhouse_core::ids::SessionId::new(session_id))
        .await
        .is_err()
}

/// F7: a namespace the upstream returns on a Messages-surface session does
/// not fork the session on the very next turn.
///
/// `how_to_prove`'s prescribed shape: a custom `FrontierClient` that emits
/// `namespace: Some(_)`, driven through the Messages router for one
/// tool-using turn, resent as history (`tool_use` + `tool_result` + new user
/// block) exactly as Claude Code's own accumulator would send it — the
/// Messages wire has no `namespace` key to resend, so the resent `tool_use`
/// block necessarily claims `namespace: None` the same way every real
/// Messages client's resend does.
///
/// Fixed: the engine's namespace join now discards a decoded namespace on a
/// `ClaudeMessages`-dialect session before it is ever stored, so the second
/// turn's resend has nothing to disagree with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_namespaced_tool_call_on_the_messages_surface_does_not_fork() {
    let (app, store) = deployment();
    let session = "f7-namespace-messages";
    let headers = [("x-claude-code-session-id", session)];

    let first_body = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [{ "role": "user", "content": "find main" }],
    });
    let first_text = post(&app, &headers, &first_body).await;
    let first = audit(&first_text)
        .unwrap_or_else(|error| panic!("the stream is not conformant: {error}\n\n{first_text}"));

    // The item the engine committed carries no namespace — the join
    // discards what `NamespacedThenText` decoded because this session is
    // `ClaudeMessages`-dialect (its key carries the `anthropic_messages`
    // segment), the opposite of `review_m17_f3.rs`'s Responses-surface
    // control, which pins the namespace surviving there. This is the fix
    // under test: a `Some(_)` reaching the join here is exactly the case
    // the finding names, and it must not be stored.
    let stored_first = store
        .read_events(
            &roundhouse_core::ids::SessionId::new(&named(session)),
            0,
            1024,
        )
        .await
        .expect("the session exists")
        .into_iter()
        .filter_map(|event| match event.kind {
            roundhouse_core::event::SessionEventKind::ItemAppended { item } => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    let call = stored_first
        .iter()
        .find(|item| {
            matches!(
                item.content,
                roundhouse_core::item::ItemContent::ToolCall { .. }
            )
        })
        .expect("the first turn committed a tool call item");
    match &call.content {
        roundhouse_core::item::ItemContent::ToolCall { namespace, .. } => {
            assert_eq!(
                namespace.as_deref(),
                None,
                "F7's fix: the Messages surface has no wire field to ever \
                 resend a stored namespace through, so the engine's join \
                 must not store the upstream's decoded {NAMESPACE:?} here \
                 in the first place -- storing it is what forked the \
                 session on the next resend before the fix: {call:?}"
            );
        }
        other => unreachable!("filtered to ToolCall above: {other:?}"),
    }

    // Turn two, exactly as Claude Code composes it: the original request, the
    // assistant message it just accumulated (its `tool_use` block carries no
    // `namespace` key — the Messages wire has none), and the tool result.
    let resent_assistant = json!({ "role": "assistant", "content": first.blocks });
    let second_body = json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "messages": [
            { "role": "user", "content": "find main" },
            resent_assistant,
            { "role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_f7",
                "content": "src/main.rs:1: fn main() {",
            }] },
        ],
    });
    let second_text = post(&app, &headers, &second_body).await;
    let _second = audit(&second_text)
        .unwrap_or_else(|error| panic!("the stream is not conformant: {error}\n\n{second_text}"));

    // The claim under test: does the resend fork to a fresh generation?
    let forked = !no_such_session(&store, &format!("{}#g1", named(session))).await;
    assert!(
        !forked,
        "F7 claims a namespace surviving onto a Messages-surface session \
         forks it on every subsequent tool-using turn; the resend above is \
         exactly the shape the finding's `how_to_prove` describes (a stored \
         `Some(namespace)` paired against a claimed `None` from the wire's \
         own `tool_use` reader), and it did not fork -- generation `#g1` was \
         never opened: {:?}",
        bound_session(&named(session), 1)
    );
}
