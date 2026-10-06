// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod review_support;

use review_support::*;
use roundhouse_core::control::{Principal, TurnPolicy};
use roundhouse_core::ids::{ResponseId, SideCallId};
use roundhouse_core::interject::{InterjectionContext, Interjector};
use roundhouse_core::item::Item;
use roundhouse_core::validate::{
    Arm, ControlCallDialect, DEFAULT_INTERVAL_SECTION_BYTES, IntervalLabel, Objective,
    ObjectiveVersion, SideCall,
};
use std::sync::Arc;

const PARTIAL: &str = "OMITTED_PARTIAL_SENTINEL";
const INSTRUCTIONS: &str = "OMITTED_INSTRUCTIONS_SENTINEL";

async fn review_history(
    include_partial: bool,
    include_instructions: bool,
) -> (String, IntervalLabel) {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log
        .begin(
            "first",
            vec![developer(INSTRUCTIONS), Item::user_text("question")],
        )
        .await;
    log.route(&first, Some(ObjectiveVersion::Undeclared)).await;
    log.fail(&first, PARTIAL).await;
    let current = log.begin("retry", Vec::new()).await;
    let mut conversation = Vec::new();
    if include_instructions {
        conversation.push(developer(INSTRUCTIONS));
    }
    conversation.push(Item::user_text("question"));
    if include_partial {
        conversation.push(Item::assistant_text(
            PARTIAL,
            ResponseId::new("client-partial"),
        ));
    }
    let judge = ScriptedJudge::answering(&[ON_TRACK]);
    let validator = validator_over(Arc::clone(&judge), DEFAULT_INTERVAL_SECTION_BYTES);
    let principal = Principal::new("acme", "ada");
    let policy = TurnPolicy::unrestricted();
    let side_call_id = SideCallId::generate();
    let terms = observing();
    let result = validator
        .consider(&InterjectionContext {
            state: log.state(),
            conversation: &conversation,
            response_id: &current,
            turn_policy: &policy,
            objective: Objective::Unknown,
            side_call: SideCall {
                session_id: &log.id,
                id: &side_call_id,
                principal: &principal,
                budget: None,
            },
            validation: Some(&terms),
            dialect: ControlCallDialect::ClaudeMessages,
        })
        .await;
    assert_eq!(judge.asked(), 1);
    (
        judge.saw(0).1,
        interval_of(&result).expect("review outcome").label,
    )
}

#[tokio::test]
async fn a_review_cannot_restore_partial_output_omitted_from_the_request() {
    let (prompt, label) = review_history(false, true).await;
    assert!(
        !prompt.contains(PARTIAL),
        "omitted output reached the judge: {prompt}"
    );
    assert_eq!(label, IntervalLabel::Unknown);
}

#[tokio::test]
async fn a_review_can_read_partial_output_the_client_supplied() {
    let (prompt, label) = review_history(true, true).await;
    assert!(prompt.contains(PARTIAL));
    assert!(prompt.contains(INSTRUCTIONS));
    assert_eq!(label, IntervalLabel::Positive);
}

#[tokio::test]
async fn a_review_cannot_restore_instructions_omitted_from_the_request() {
    let (prompt, label) = review_history(true, false).await;
    assert!(
        !prompt.contains(INSTRUCTIONS),
        "omitted instructions reached the judge: {prompt}"
    );
    assert_eq!(label, IntervalLabel::Unknown);
}
