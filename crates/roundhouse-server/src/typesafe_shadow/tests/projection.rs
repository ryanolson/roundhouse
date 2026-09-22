// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What actually leaves the deployment, asserted on the bytes that arrived.
//!
//! The projection's own shape is tested in `roundhouse_core::classify`. These
//! are about the boundary: that the cap refuses rather than cuts, and that what
//! reaches a socket is the projection and not the conversation.

use super::*;
use roundhouse_core::item::{ItemContent, Role};

fn tool_result(call_id: &str, output: &str) -> Item {
    Item {
        role: Role::Tool,
        content: ItemContent::ToolResult {
            call_id: call_id.into(),
            output: output.into(),
        },
        response_id: None,
    }
}

/// **The state on the wire is the prompt, and neither the instructions nor the
/// tool output beside it.**
///
/// Asserted on the body that arrived rather than on what the helper returned:
/// the projection could be right and still be the wrong thing to serialize.
#[tokio::test]
async fn the_sent_state_carries_the_prompt_and_nothing_the_ruling_excludes() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let shadow = shadow(addr, config().enable(), ledger);

    let input = vec![
        Item::system_text("You are working in /srv/secret-project with credentials in .env."),
        tool_result("call_1", "DB_PASSWORD=hunter2"),
        Item::user_text("the parser drops trailing commas; fix it and prove it"),
    ];
    let projection = shadow
        .projection(&PromptCapture::of(&input, &caps()), &[], &[])
        .expect("it fits");
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    shadow.execute(prepared, never()).await;

    let state = up.state();
    assert!(state.contains("trailing commas"), "{state}");
    assert!(
        !state.contains("secret-project"),
        "the deployment's own instructions must not leave it:\n{state}"
    );
    assert!(
        !state.contains("hunter2"),
        "raw tool output must not leave it:\n{state}"
    );
    assert!(
        state.contains("2 items omitted"),
        "and what was withheld is counted rather than silent:\n{state}"
    );
}

/// A tool continuation says so, rather than arriving as an empty prompt a
/// classifier would read as a trivial request.
#[tokio::test]
async fn a_tool_continuation_is_stated_on_the_wire() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let shadow = shadow(addr, config().enable(), ledger);

    let input = vec![tool_result("call_1", "3 tests passed")];
    let projection = shadow
        .projection(&PromptCapture::of(&input, &caps()), &[], &[])
        .expect("it fits");
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    shadow.execute(prepared, never()).await;

    let state = up.state();
    assert!(state.contains("tool_continuation"), "{state}");
    assert!(
        !state.contains("3 tests passed"),
        "the result itself still does not go:\n{state}"
    );
}

/// **Over the bound, no call happens.** Rejected, never cut: slicing the
/// rendered projection would cut the line quoting that contains a hostile
/// prompt.
#[tokio::test]
async fn an_oversized_projection_makes_no_call_and_takes_no_hold() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let tight = ShadowConfig::new(
        "jev-1.12",
        pricing(),
        EXPECTED_OUTPUT_TOKENS,
        ProjectionCaps {
            max_prior_classifications: 4,
            max_prompt_chars: 4_000,
            max_total_bytes: 200,
        },
        CONFIG_REVISION,
    )
    .enable();
    let shadow = shadow(addr, tight, ledger.clone());

    let input = vec![Item::user_text("x".repeat(4_000))];
    let refusal = shadow
        .projection(&PromptCapture::of(&input, &shadow.config().caps), &[], &[])
        .expect_err("it does not fit");

    assert!(
        matches!(refusal, NotRun::PayloadTooLarge { limit_bytes, .. } if limit_bytes == 200),
        "{refusal:?}"
    );
    assert_eq!(up.count(), 0);
    assert!(
        ledger.requested().is_empty(),
        "no hold is opened for a call that cannot be made"
    );
}

/// The transport's own wire bound is separate from the projection's, and a
/// request over it is refused before a socket and before a grant.
#[tokio::test]
async fn a_body_over_the_transport_bound_is_refused_before_the_hold() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    // A projection cap that permits far more than the wire limit does, so the
    // refusal can only come from the transport.
    let roomy = ShadowConfig::new(
        "jev-1.12",
        pricing(),
        EXPECTED_OUTPUT_TOKENS,
        ProjectionCaps {
            max_prior_classifications: 4,
            max_prompt_chars: 40_000,
            max_total_bytes: 64 * 1024,
        },
        CONFIG_REVISION,
    )
    .enable();
    let client = SystemOneClient::new(
        format!("http://{addr}"),
        SystemOneLimits {
            max_request_bytes: 256,
            ..limits()
        },
    )
    .unwrap();
    let shadow = TypeSafeShadow::new(client, roomy, ledger.clone(), ByteTokenizer);

    let input = vec![Item::user_text("y".repeat(4_000))];
    let projection = shadow
        .projection(&PromptCapture::of(&input, &shadow.config().caps), &[], &[])
        .expect("the projection cap allows it");
    let refusal = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect_err("the wire bound does not");

    assert!(
        matches!(
            refusal,
            NotRun::Refused(roundhouse_fleet::typesafe::SystemOneError::RequestTooLarge { .. })
        ),
        "{refusal:?}"
    );
    assert_eq!(up.count(), 0);
    assert!(
        ledger.requested().is_empty(),
        "everything the transport can refuse without a socket is an \
         eligibility question, and answering it with a hold open charges the \
         evaluation ledger for a call that was never going to happen"
    );
}

/// Prior classifications ride to the service as metadata, which is the whole
/// point of a feature that enriches later turns.
#[tokio::test]
async fn prior_classifications_reach_the_service_as_metadata() {
    use roundhouse_core::classify::{
        AvailableClassification, ClassificationRef, ContextDependence, Graded, TAXONOMY_VERSION,
        TurnClassification, TurnComplexity, TurnIntent,
    };

    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let shadow = shadow(addr, config().enable(), ledger);

    let prior = vec![AvailableClassification {
        reference: ClassificationRef {
            call_id: ResponseId::new("call_earlier"),
            source_turn_index: 2,
            available_seq: 40,
        },
        classification: TurnClassification {
            taxonomy_version: TAXONOMY_VERSION,
            intent: Graded {
                value: TurnIntent::Diagnose,
                confidence: 0.9,
            },
            complexity: Graded {
                value: TurnComplexity::Deep,
                confidence: 0.8,
            },
            context_dependence: Graded {
                value: ContextDependence::SelfContained,
                confidence: 0.7,
            },
        },
    }];
    let projection = shadow.projection(&capture(), &prior, &[]).expect("it fits");
    assert_eq!(projection.prior_included, 1);
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    shadow.execute(prepared, never()).await;

    let state = up.state();
    assert!(
        state.contains("turn 2: intent=diagnose complexity=deep context=self_contained"),
        "{state}"
    );
}
