// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review finding G11 (M10 thermo-nuclear round): does a provider-reported
//! dollar figure survive the turn that earned it?
//!
//! M10.1's P3 ruling: OpenRouter's `cost` field rides "as a sidecar on the
//! decision/settle record" — a price is not a token count, and the
//! reconciliation view (M10.3(4)) is its consumer. What shipped parses the
//! field onto `FrontierChunk::Done`, then `engine.rs` only `tracing::debug!`s
//! it — a level the binary's own default `EnvFilter` ("info") drops — and
//! records it nowhere durable. This file is the proving test: it runs a real
//! turn whose `Done` frame carries a provider-reported cost and checks
//! whether that number is readable from the session's routing/settlement
//! record afterward.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{
    FrontierChunk, FrontierClient, FrontierError, FrontierQuote, FrontierStream,
};
use roundhouse_server::{Admission, EchoLocalExecutor, Engine};
use tokio::sync::mpsc;

mod common;
use common::{config, frontier_catalog};

/// A [`FrontierClient`] whose stream the test feeds one chunk at a time —
/// copied from `turn_lifecycle.rs::PacedFrontierClient` rather than shared,
/// because this file must stand alone as review evidence and not shift if
/// that suite's fixture changes shape later.
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

/// G11: a provider-reported price must survive the turn that earned it,
/// readable from the durable log — not only from a `tracing::debug!` line the
/// binary's own default filter drops.
///
/// **Asserted against `ResponseCompleted`, not against `DecisionRecord`**, and
/// the move is the finding's own mechanism read one step further: `Routed` is
/// written *before* the dispatch is attempted, and the provider's price arrives
/// on the stream's last frame, so the decision record cannot learn it without
/// mutating a committed event. The settle side is where it can land, and
/// reading it back out of the store rather than off an in-memory projection is
/// the stronger claim — it proves the number survives the process that measured
/// it, which is the whole of what P3 asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_reported_price_survives_the_turn_that_earned_it() {
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
                    vec![Item::user_text("price it")],
                    &Admission::open(),
                )
                .await
        }
    });

    chunks
        .send(Ok(FrontierChunk::OutputText("answer".into())))
        .await
        .unwrap();
    // The number OpenRouter would have put on the final usage frame of a real
    // response — see `openai_responses/stream.rs:219`.
    chunks
        .send(Ok(FrontierChunk::Done {
            input_tokens: 40,
            cached_input_tokens: 0,
            output_tokens: 3,
            reasoning_tokens: 0,
            provider_reported_cost: Some(0.00421),
        }))
        .await
        .unwrap();
    drop(chunks);

    running.await.unwrap().expect("the stream completed");

    let events = store.read_events(&session_id, 0, 1024).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::Routed { .. })),
        "the turn was routed"
    );
    let reported = events
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::ResponseCompleted {
                provider_reported_cost_usd,
                ..
            } => Some(*provider_reported_cost_usd),
            _ => None,
        })
        .expect("the turn completed");

    assert_eq!(
        reported,
        Some(0.00421),
        "P3 rules this a sidecar on the decision/settle record; a \
         tracing::debug! below the binary's own default filter is neither a \
         record nor visible",
    );

    // Durable, not merely in-memory: the same bytes a successor process would
    // replay. This is the property the `debug!` never had.
    let replayed: SessionEventKind = serde_json::from_str(
        &serde_json::to_string(
            &events
                .iter()
                .find(|event| matches!(event.kind, SessionEventKind::ResponseCompleted { .. }))
                .unwrap()
                .kind,
        )
        .unwrap(),
    )
    .unwrap();
    let SessionEventKind::ResponseCompleted {
        provider_reported_cost_usd,
        ..
    } = replayed
    else {
        unreachable!("filtered to the completed event")
    };
    assert_eq!(provider_reported_cost_usd, Some(0.00421));
}

/// The control: a turn whose provider reports nothing records `None`, not a
/// confident zero. A provider that bills nothing and a provider that says
/// nothing are different facts, and the reconciliation view publishes only the
/// first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_provider_records_no_price_rather_than_zero() {
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
                    vec![Item::user_text("price it")],
                    &Admission::open(),
                )
                .await
        }
    });

    chunks
        .send(Ok(FrontierChunk::OutputText("answer".into())))
        .await
        .unwrap();
    chunks
        .send(Ok(FrontierChunk::Done {
            input_tokens: 40,
            cached_input_tokens: 0,
            output_tokens: 3,
            reasoning_tokens: 0,
            provider_reported_cost: None,
        }))
        .await
        .unwrap();
    drop(chunks);

    running.await.unwrap().expect("the stream completed");

    let events = store.read_events(&session_id, 0, 1024).await.unwrap();
    let completed = events
        .iter()
        .find(|event| matches!(event.kind, SessionEventKind::ResponseCompleted { .. }))
        .expect("the turn completed");
    let SessionEventKind::ResponseCompleted {
        provider_reported_cost_usd,
        ..
    } = &completed.kind
    else {
        unreachable!("filtered to the completed event")
    };
    assert_eq!(*provider_reported_cost_usd, None);
    assert!(
        !serde_json::to_string(&completed.kind)
            .unwrap()
            .contains("provider_reported_cost_usd"),
        "skipped on the wire when absent, so a deployment whose upstreams stay \
         silent writes the bytes it wrote before the field existed"
    );
}
