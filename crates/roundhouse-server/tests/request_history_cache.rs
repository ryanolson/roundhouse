// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use roundhouse_core::event::SessionEventKind;
use roundhouse_core::ids::{ResponseId, SessionId, TurnId};
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::store::{MemoryStore, SessionStore};
use roundhouse_fleet::{EchoFrontierClient, WireProtocol};
use roundhouse_server::test_support::{engine_over_echo, frontier_spec, single_model_catalog};
use roundhouse_server::{Admission, EngineConfig, TurnHistory, TurnInput};

async fn cache_estimates_for_requests(instructions: &[bool]) -> Vec<u64> {
    let store = Arc::new(MemoryStore::new());
    let engine = engine_over_echo(
        Arc::clone(&store),
        single_model_catalog(frontier_spec(
            "cache-history",
            "m",
            WireProtocol::OpenAiResponses,
        )),
        Arc::new(EchoFrontierClient::new("answer")),
        EngineConfig::default(),
    );
    let session = SessionId::new("cache-history");
    engine.create_session(&session).await.unwrap();
    let configuration = Item {
        role: Role::Developer,
        content: ItemContent::Text {
            text: "instructions ".repeat(200),
        },
        response_id: None,
    };
    let mut history = Vec::new();
    let mut estimates = Vec::new();
    for (index, keep_instructions) in instructions.iter().copied().enumerate() {
        let mut prior = history.clone();
        let question = Item::user_text(format!("question {index}"));
        let mut delta = vec![question.clone()];
        if keep_instructions {
            if index == 0 {
                delta.insert(0, configuration.clone());
            } else {
                prior.insert(0, configuration.clone());
            }
        }
        let mut complete = prior;
        complete.extend(delta.clone());
        engine
            .run_turn(
                &session,
                TurnId::new(format!("turn-{index}")),
                TurnInput {
                    items: delta,
                    history: TurnHistory::Complete(complete),
                    ..TurnInput::from(Vec::new())
                },
                &Admission::open(),
            )
            .await
            .unwrap();
        history.push(question);
        history.push(Item::assistant_text(
            "answer",
            ResponseId::new(format!("client-{index}")),
        ));
        let events = store.read_events(&session, 0, 1024).await.unwrap();
        estimates.push(
            events
                .iter()
                .rev()
                .find_map(|event| match &event.kind {
                    SessionEventKind::Routed { decision, .. } => Some(
                        decision
                            .considered
                            .iter()
                            .find(|candidate| candidate.target == decision.chosen)
                            .expect("chosen candidate")
                            .matched_prefix_tokens,
                    ),
                    _ => None,
                })
                .expect("routing decision"),
        );
    }
    estimates
}

#[tokio::test]
async fn omitted_instructions_do_not_claim_the_previous_prefix_is_cached() {
    assert_eq!(cache_estimates_for_requests(&[true, false]).await[1], 0);
}

#[tokio::test]
async fn unchanged_history_keeps_its_cache_estimate() {
    assert!(cache_estimates_for_requests(&[true, true]).await[1] > 0);
}

#[tokio::test]
async fn replay_does_not_restore_an_unverified_cache_prediction() {
    let estimates = cache_estimates_for_requests(&[true, false, true, true]).await;
    assert_eq!(estimates[2], 0);
    assert!(estimates[3] > 0);
}
