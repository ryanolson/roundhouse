// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review finding F5 (M11.2a thermo-nuclear round): does the dispatch fold's
//! unreported-usage fallback count what a tool-only turn actually produced?
//!
//! `Engine::estimated_usage` (`engine.rs:1685-1696`) is fed `spoken`
//! (`engine.rs:1567`) — the accumulator that gathers only
//! `FrontierChunk::OutputText` (`engine.rs:1449`), never a tool call's
//! `id`/`name`/`arguments` (committed on a separate path, `engine.rs:1468-
//! 1512`). A turn whose whole answer is tool calls has `spoken == ""`, so the
//! fallback's `self.tokenizer.encode(text).len()` is not merely imprecise on
//! that turn, it is structurally zero.
//!
//! R2's own ruling (`agent-docs/PLAN-anthropic-messages.md:145-157`) records
//! that the Anthropic dispatch client emits **no** `Done` at all for a stream
//! that never completes, specifically so the estimate — not a fabricated
//! zero-token `Done` — stays authoritative, because "a zero-token `Done`
//! reads as a saving" is named there as "the one failure the metrics chapter
//! is built against". This file checks whether the estimate the design
//! deliberately routes around that failure to reaches recreates it anyway for
//! the one turn shape `spoken` cannot see. The sibling `context_contribution`
//! (`engine.rs:1653-1671`) gets the same question right forty lines away, on
//! `emitted.render()` rather than spoken text, "because a tool call says
//! nothing to a human but occupies context exactly as `plan` will count it
//! when the client resends it next turn" (`engine.rs:1664-1667`) — this file
//! runs that exact sentence's reasoning back over the fallback's own output.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::{Accounting, SessionEventKind};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent};
use roundhouse_core::routing::{AffinityPolicy, CacheLedger, ProviderPricing};
use roundhouse_core::session::Session;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
};
use roundhouse_server::{Admission, EchoLocalExecutor, Engine};
use tokio::sync::mpsc;

mod common;
use common::{config, frontier_catalog};

/// A [`FrontierClient`] whose stream the test feeds one chunk at a time —
/// copied from `turn_lifecycle.rs`/`review_m10_g11.rs`'s twin rather than
/// shared, for the reason both of those already give: this file must stand
/// alone as review evidence and not shift if that fixture changes shape.
struct PacedFrontierClient {
    chunks: Mutex<Option<mpsc::Receiver<Result<FrontierChunk, FrontierError>>>>,
}

impl PacedFrontierClient {
    fn new() -> (Self, mpsc::Sender<Result<FrontierChunk, FrontierError>>) {
        let (sender, receiver) = mpsc::channel(8);
        (
            Self {
                chunks: Mutex::new(Some(receiver)),
            },
            sender,
        )
    }
}

#[async_trait]
impl FrontierClient for PacedFrontierClient {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        let receiver =
            self.chunks.lock().unwrap().take().ok_or_else(|| {
                FrontierError::Upstream("the paced client answers one turn".into())
            })?;
        Ok(
            futures::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|chunk| (chunk, receiver))
            })
            .boxed(),
        )
    }
}

/// The rate card `common::frontier_catalog()` prices its one model at —
/// mirrored rather than looked up, so this file's dollar assertion does not
/// need a catalog accessor. `engine::spend::settled_cost_usd` (`spend.rs:299`)
/// is `pub(super)` and unreachable from an integration test, but it is a thin
/// wrapper: for a routed frontier target it is exactly `card.price(&usage)`
/// (`spend.rs:304`), and `ProviderPricing::price`/`price_tokens` are the
/// public functions it delegates to — calling them here on this mirrored card
/// reproduces the real settle arithmetic rather than approximating it.
fn rate_card() -> ProviderPricing {
    ProviderPricing {
        input_per_mtok_usd: 3.0,
        cached_input_per_mtok_usd: 0.3,
        cache_write_per_mtok_usd: 3.75,
        output_per_mtok_usd: 15.0,
    }
}

/// F5: a turn that is entirely tool calls, whose provider reports no terminal
/// usage, settles at exactly zero output tokens — and prices at exactly zero
/// output dollars — despite dispatching two real tool calls with substantial
/// arguments.
///
/// **The stream ends with no `Done` at all**, which is not a contrived
/// malformed frame: it is R2's own documented shape for "unreported"
/// (`agent-docs/PLAN-anthropic-messages.md:154-157`, "a stream that never
/// completes still yields no `Done`"), the exact case the fallback exists to
/// survive.
///
/// **Ruled valid, and fixed.** `estimated_usage` now sums the same
/// `render()`-based measure `context_contribution` already applies to a
/// committed call, over every tool call this dispatch committed, added to the
/// tokenized `spoken` text — see `Engine::estimated_usage` and the
/// `tool_call_output_tokens` accumulator in `Engine::dispatch`. Confirmed
/// failing at b8e8ddd (`result.usage.output_tokens == 0` against an honest
/// (`context_contribution`) count of 218 tokens for the same two committed
/// calls) before the fix; kept live as the guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_only_turn_with_unreported_usage_settles_at_zero_output_tokens() {
    let store = Arc::new(MemoryStore::new());
    let (client, chunks) = PacedFrontierClient::new();
    let engine = Arc::new(Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(client),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let running = tokio::spawn({
        let engine = Arc::clone(&engine);
        let session_id = session_id.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    TurnId::new("t0"),
                    vec![Item::user_text("find and read main")],
                    &Admission::open(),
                )
                .await
        }
    });

    // Two real tool calls, arguments long enough that no honest count of what
    // was produced reads as zero — and then the connection ends with no
    // `Done` at all.
    chunks
        .send(Ok(FrontierChunk::ToolCall {
            id: "toolu_01".into(),
            name: "Grep".into(),
            namespace: None,
            arguments: r#"{"pattern": "fn main", "path": "/src", "output_mode": "content"}"#.into(),
        }))
        .await
        .unwrap();
    chunks
        .send(Ok(FrontierChunk::ToolCall {
            id: "toolu_02".into(),
            name: "Read".into(),
            namespace: None,
            arguments: r#"{"path": "/src/main.rs", "limit": 200}"#.into(),
        }))
        .await
        .unwrap();
    drop(chunks); // no `Done` — the unreported-usage shape

    let result = running
        .await
        .unwrap()
        .expect("the turn completes: a stream with no Done is not a stream error");

    // Premise checks — these must PASS, and establish that the turn really
    // did produce real, non-empty content and that the fallback really did
    // fire, so the assertion below is about the fallback's *count* rather
    // than about an empty turn or the reported-usage path.
    assert_eq!(
        result.usage.accounting,
        Accounting::Estimated,
        "the fallback must have fired — this is what the test is about"
    );
    let probe = Session::open(store, session_id, "probe", 10_000, CacheLedger::new())
        .await
        .unwrap();
    let calls: Vec<Item> = probe
        .state()
        .items
        .iter()
        .filter(|item| matches!(item.content, ItemContent::ToolCall { .. }))
        .cloned()
        .collect();
    assert_eq!(calls.len(), 2, "both calls were committed to the log");
    // The sibling function in the same file, applied to the very items this
    // turn produced: `context_contribution` counts `emitted.render()` rather
    // than spoken text, and is non-zero here — proving there was real content
    // for an honest estimate to count.
    let honest_tokens: u64 = calls
        .iter()
        .map(|item| engine.context_contribution(0, item).output_tokens)
        .sum();
    assert!(
        honest_tokens > 0,
        "premise: the sibling function proves real content existed to count \
         ({honest_tokens} tokens across the two committed calls)"
    );

    // THE DEFECT. F5's claim: a turn that dispatched two real tool calls,
    // with no spoken text, settles its estimated usage at zero output
    // tokens — because `estimated_usage` (engine.rs:1685-1696) is fed
    // `spoken` (engine.rs:1567), which only ever accumulates
    // `FrontierChunk::OutputText` (engine.rs:1449) and never a tool call's
    // content. The correct estimate is non-zero, matching what the sibling
    // `context_contribution` computes for the same items (`honest_tokens`
    // above) — this is the assertion that must fail on the unfixed code.
    assert!(
        result.usage.output_tokens > 0,
        "F5: two real tool calls were dispatched (honest count: {honest_tokens} \
         tokens) but the fallback settled this turn's output at {} tokens — \
         estimated_usage counts only `spoken`, which a tool-only turn never \
         populates",
        result.usage.output_tokens
    );

    // Wrong stored state: the durable event a successor replays should carry
    // the same non-zero count, not merely the in-memory TurnResult.
    let events = probe.events_since(0, 1000).await.unwrap();
    let stored_usage = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::ResponseCompleted { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("the turn completed and recorded its usage");
    assert!(
        stored_usage.output_tokens > 0,
        "F5: the durable ResponseCompleted event also stores the zero — a \
         successor replaying this session's log sees the same wrong count"
    );
    assert_eq!(stored_usage.accounting, Accounting::Estimated);

    // Wrong money: the exact formula `engine::spend::settled_cost_usd`
    // delegates to for a routed frontier target, applied to this turn's own
    // settled usage and isolated to the output axis.
    let card = rate_card();
    let priced_output_usd = card.price_tokens(0.0, 0.0, result.usage.output_tokens as f64);
    assert!(
        priced_output_usd > 0.0,
        "F5: the settle path's own pricing formula charges exactly $0.00 for \
         this dispatch's output, though it produced two real tool calls — \
         indistinguishable downstream from a routing saving"
    );
}

/// CONTROL: the same unreported-usage shape — a stream with no `Done` at all
/// — but the turn spoke prose instead of calling a tool. The fallback
/// estimates real, non-zero output tokens here, which is what proves the zero
/// above is specific to the tool-only shape and not a general breakage of the
/// fallback (which would make the finding above tautological rather than
/// about tool calls specifically).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prose_only_turn_with_unreported_usage_estimates_real_output_tokens() {
    let store = Arc::new(MemoryStore::new());
    let (client, chunks) = PacedFrontierClient::new();
    let engine = Arc::new(Engine::new(
        store.clone(),
        ByteTokenizer,
        Arc::new(EchoLocalExecutor::new("local answer")),
        frontier_catalog(),
        Arc::new(client),
        Arc::new(AffinityPolicy::new()),
        config(),
    ));
    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();

    let running = tokio::spawn({
        let engine = Arc::clone(&engine);
        let session_id = session_id.clone();
        async move {
            engine
                .run_turn(
                    &session_id,
                    TurnId::new("t0"),
                    vec![Item::user_text("say something")],
                    &Admission::open(),
                )
                .await
        }
    });

    chunks
        .send(Ok(FrontierChunk::OutputText(
            "a genuinely long spoken answer with real words in it".into(),
        )))
        .await
        .unwrap();
    drop(chunks); // no Done here either

    let result = running.await.unwrap().expect("the turn completes");

    assert_eq!(result.usage.accounting, Accounting::Estimated);
    assert!(
        result.usage.output_tokens > 0,
        "the control: the same fallback, fed real spoken text, does the \
         honest thing — isolating F5 to the tool-only shape"
    );
}
