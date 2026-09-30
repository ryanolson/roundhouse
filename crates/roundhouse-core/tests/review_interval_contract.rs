// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod review_support;

use review_support::*;
use roundhouse_core::item::Item;
use roundhouse_core::validate::{
    Arm, DEFAULT_INTERVAL_SECTION_BYTES, IntervalLabel, IntervalReview, Objective, ObjectiveVersion,
};

async fn control_review(output: &str) -> (String, IntervalReview) {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log
        .begin(
            "t0",
            vec![Item::user_text("implement the requested change")],
        )
        .await;
    log.route(&first, Some(ObjectiveVersion::Undeclared)).await;
    log.emit(
        &first,
        Item::tool_call("ours", "mcp__roundhouse__explain_last_route", "{}"),
    )
    .await;
    log.session
        .complete(&first, None, usage(), None, None)
        .await
        .unwrap();
    let judge = ScriptedJudge::answering(&[ON_TRACK]);
    let validator = validator_over(judge.clone(), DEFAULT_INTERVAL_SECTION_BYTES);
    let (_, decided) = review_turn(
        &mut log,
        &validator,
        &observing(),
        "t1",
        vec![tool_result("ours", output)],
        Objective::Unknown,
    )
    .await;
    let (system, brief) = judge.saw(0);
    (
        format!("{system}\n{brief}"),
        interval_of(&decided).expect("review record"),
    )
}

#[tokio::test]
async fn roundhouse_routing_facts_are_absent_from_the_entire_judge_prompt() {
    let output = format!(
        "chosen: {ROUTED_PROVIDER}/{ROUTED_MODEL}; price: {ROUTED_PRICE}; rationale: {ROUTED_RATIONALE}"
    );
    let (prompt, review) = control_review(&output).await;
    let leaked: Vec<_> = [ROUTED_PROVIDER, ROUTED_MODEL, "0.4271", ROUTED_RATIONALE]
        .into_iter()
        .filter(|value| prompt.contains(value))
        .collect();
    assert!(
        leaked.is_empty(),
        "Roundhouse routing facts leaked: {leaked:?}; label={:?}; gaps={:?}\n{prompt}",
        review.label,
        review.gaps
    );
}

#[tokio::test]
async fn withheld_content_beyond_the_classic_head_makes_the_interval_unknown() {
    let marker = "ESSENTIAL-CONTROL-RESULT-TAIL";
    let output = format!("{} {marker}", "x".repeat(300));
    let (prompt, review) = control_review(&output).await;
    assert!(
        !prompt.contains(marker),
        "fixture requires actual omitted content"
    );
    assert!(prompt.contains("withheld from this review"));
    assert!(!review.decisions.is_empty());
    assert_eq!(
        review.label,
        IntervalLabel::Unknown,
        "content is absent from both sections, so a positive verdict cannot certify complete coverage; gaps={:?}",
        review.gaps
    );
}

#[tokio::test]
async fn ordinary_user_model_words_and_own_tool_results_remain_reviewable() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log
        .begin(
            "t0",
            vec![Item::user_text(format!("document {ROUTED_MODEL}"))],
        )
        .await;
    log.route(&first, Some(ObjectiveVersion::Undeclared)).await;
    log.emit(
        &first,
        Item::tool_call("theirs", "explain_last_route", "{}"),
    )
    .await;
    log.session
        .complete(&first, None, usage(), None, None)
        .await
        .unwrap();
    let judge = ScriptedJudge::answering(&[ON_TRACK]);
    let validator = validator_over(judge.clone(), DEFAULT_INTERVAL_SECTION_BYTES);
    let (_, decided) = review_turn(
        &mut log,
        &validator,
        &observing(),
        "t1",
        vec![tool_result("theirs", "OWN-TOOL-OUTPUT")],
        Objective::Unknown,
    )
    .await;
    let brief = judge.saw(0).1;
    let shown = section(&brief).expect("complete interval");
    assert!(shown.contains(ROUTED_MODEL));
    assert!(shown.contains("OWN-TOOL-OUTPUT"));
    let review = interval_of(&decided).unwrap();
    assert!(review.gaps.is_empty());
    assert_eq!(review.label, IntervalLabel::Positive);
}
