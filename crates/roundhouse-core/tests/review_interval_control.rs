// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Roundhouse's own control traffic never reaches the judge, and an interval
//! that holds some is never labelled.
//!
//! A control result can carry the chosen target, its rationale and the policy
//! digest. `explain_last_route` returns exactly those. So the classic brief
//! names such a step and shows nothing else of it. The validator also leaves
//! out the whole reviewed-turns section. A verdict over contents that the
//! judge did not see is not a review of the interval, so the label is unknown.
//!
//! **These tests examine pairing in both directions.** A test that looks only
//! for missing routing facts passes a build that withholds every tool result.
//! So the controls here are results that the prompt must still show:
//!
//! - a result whose call was the agent's own tool, before the interval
//! - a result whose id an earlier control call also used

mod review_support;

use roundhouse_core::ids::ResponseId;
use roundhouse_core::interject::Interjection;
use roundhouse_core::item::Item;
use roundhouse_core::validate::{
    Arm, CONTROL_TOOL_NAMESPACE, ControlCallDialect, CoverageGap, DEFAULT_INTERVAL_SECTION_BYTES,
    IntervalLabel, IntervalReview, Objective, ObjectiveVersion, Validator,
};

use review_support::*;

/// Roundhouse's own tool, as a Claude Code session stores the call.
const CONTROL_CALL: &str = "mcp__roundhouse__explain_last_route";

/// What `explain_last_route` answers with: every routing fact the judge must
/// not see.
fn routing_answer() -> String {
    format!(
        "chosen: {ROUTED_PROVIDER}/{ROUTED_MODEL}\nprice: {ROUTED_PRICE}\n\
         rationale: {ROUTED_RATIONALE}\npolicy: {ROUTED_DIGEST}"
    )
}

/// The routing facts and control arguments that appear in the whole prompt.
fn leaks(prompt: &str) -> Vec<String> {
    [
        ROUTED_PROVIDER.to_string(),
        ROUTED_MODEL.to_string(),
        ROUTED_PRICE.to_string(),
        ROUTED_RATIONALE.to_string(),
        ROUTED_DIGEST.to_string(),
        "CONTROL-ARGUMENTS".to_string(),
    ]
    .into_iter()
    .filter(|value| prompt.contains(value.as_str()))
    .collect()
}

/// Admit `turn`, review it on `dialect`, and commit the review.
async fn review_on(
    log: &mut Log,
    validator: &Validator,
    turn: &str,
    input: Vec<Item>,
    dialect: ControlCallDialect,
) -> (ResponseId, IntervalReview) {
    let response = log.begin(turn, input).await;
    let decided: Interjection = consider_on(
        validator,
        &observing(),
        log.state(),
        &response,
        Objective::Unknown,
        dialect,
    )
    .await;
    log.commit(&response, decided.clone()).await;
    (response, interval_of(&decided).expect("a parsed review"))
}

async fn review(
    log: &mut Log,
    validator: &Validator,
    turn: &str,
    input: Vec<Item>,
) -> (ResponseId, IntervalReview) {
    review_on(
        log,
        validator,
        turn,
        input,
        ControlCallDialect::ClaudeMessages,
    )
    .await
}

/// An answered turn that made one tool call.
async fn calling_turn(log: &mut Log, response: &ResponseId, call: Item) {
    log.route(response, Some(ObjectiveVersion::Undeclared))
        .await;
    log.emit(response, call).await;
    log.session
        .complete(response, None, usage(), None, None)
        .await
        .unwrap();
}

/// **The contract.** Control traffic inside the interval, and a result whose
/// control call came before it, both leave the interval unknown. The whole
/// prompt carries none of the arguments or the answer of the call. The
/// validator leaves out the section instead of showing it with holes.
///
/// A positive label here is wrong even with a marker where each control call
/// was. The label then certifies content that the judge never saw, and a label
/// must never do that.
#[tokio::test]
async fn control_traffic_leaves_the_interval_unknown_and_its_contents_out_of_the_prompt() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log
        .begin("t0", vec![Item::user_text("why did that go there?")])
        .await;
    calling_turn(
        &mut log,
        &first,
        Item::tool_call("ours", CONTROL_CALL, r#"{"why":"CONTROL-ARGUMENTS"}"#),
    )
    .await;
    let judge = ScriptedJudge::answering(&[ON_TRACK, ON_TRACK]);
    let validator = validator_over(judge.clone(), DEFAULT_INTERVAL_SECTION_BYTES);
    // The call is inside this interval.
    let (second, inside) = review(
        &mut log,
        &validator,
        "t1",
        vec![tool_result("ours", &routing_answer())],
    )
    .await;
    // The agent's own tool with the same bare name, which is its work.
    calling_turn(
        &mut log,
        &second,
        Item::tool_call("theirs", "explain_last_route", r#"{"file":"OWN-ARGS"}"#),
    )
    .await;
    // This interval opens with the result of the control call, which the
    // agent made before this interval.
    let (_, before) = review(
        &mut log,
        &validator,
        "t2",
        vec![tool_result("theirs", "OWN-TOOL-OUTPUT")],
    )
    .await;

    for (n, review) in [inside, before].into_iter().enumerate() {
        assert!(!review.decisions.is_empty(), "review {n} covered decisions");
        assert_eq!(
            review.gaps,
            vec![CoverageGap::WithheldControlTraffic],
            "review {n}"
        );
        assert_eq!(review.label, IntervalLabel::Unknown, "review {n}");
        let (system, brief) = judge.saw(n);
        let prompt = format!("{system}\n{brief}");
        assert!(
            leaks(&prompt).is_empty(),
            "review {n} leaked {:?}:\n{prompt}",
            leaks(&prompt)
        );
        assert!(
            section(&brief).is_none(),
            "review {n}: a section with withheld contents is left out whole:\n{brief}"
        );
        // The brief names the step where it happened, so the judge knows that
        // the agent took a step.
        assert!(brief.contains(CONTROL_CALL), "review {n}:\n{brief}");
        assert!(
            brief.contains("withheld from this review"),
            "review {n}:\n{brief}"
        );
    }
    // The agent's own tool keeps its result in the classic brief.
    assert!(judge.saw(1).1.contains("OWN-TOOL-OUTPUT"));

    // Both reviews are checkpoints. An unknown label closes an interval. It
    // does not leave the interval open.
    let replayed = log.replay().await;
    assert_eq!(replayed.accepted_reviews(), 2);
    assert_eq!(replayed.rejected_reviews(), 0);
    assert!(
        replayed
            .review_outcomes()
            .iter()
            .all(|outcome| outcome.label == IntervalLabel::Unknown)
    );
}

/// **Control: a result whose call came before the interval is shown when the
/// call was the agent's own.**
///
/// A build that withholds every result paired with earlier history passes the
/// test above and fails this one.
#[tokio::test]
async fn a_result_whose_own_tool_call_came_before_the_interval_is_shown() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log.begin("t0", vec![Item::user_text("start")]).await;
    calling_turn(
        &mut log,
        &first,
        Item::tool_call("theirs", "explain_last_route", "{}"),
    )
    .await;
    let judge = ScriptedJudge::answering(&[ON_TRACK, ON_TRACK]);
    let validator = validator_over(judge.clone(), DEFAULT_INTERVAL_SECTION_BYTES);
    let (second, _) = review(
        &mut log,
        &validator,
        "t1",
        vec![tool_result("theirs", "OWN-OUTPUT-EARLIER")],
    )
    .await;
    log.route(&second, Some(ObjectiveVersion::Undeclared)).await;
    log.complete(&second, "checked").await;
    let (_, later) = review(&mut log, &validator, "t2", vec![Item::user_text("next")]).await;

    assert!(later.gaps.is_empty(), "{:?}", later.gaps);
    assert_eq!(later.label, IntervalLabel::Positive);
    let brief = judge.saw(1).1;
    let shown = section(&brief).unwrap_or_else(|| panic!("no section:\n{brief}"));
    assert!(
        shown.contains("result of call `theirs`") && shown.contains("OWN-OUTPUT-EARLIER"),
        "the result opens this interval and its call is not ours:\n{shown}"
    );
}

/// **Control: a result pairs with the latest call that has its id.**
///
/// An earlier control call used the same id. The result answers the agent's
/// own later call, as [`exchanges`](roundhouse_core::validate::exchanges)
/// pairs them, so the result is the agent's work.
#[tokio::test]
async fn a_result_pairs_with_the_latest_call_that_has_its_id() {
    let mut log = Log::enrolled(Some(Arm::Shadow)).await;
    let first = log.begin("t0", vec![Item::user_text("start")]).await;
    calling_turn(&mut log, &first, Item::tool_call("x", CONTROL_CALL, "{}")).await;
    let judge = ScriptedJudge::answering(&[ON_TRACK, ON_TRACK]);
    let validator = validator_over(judge.clone(), DEFAULT_INTERVAL_SECTION_BYTES);
    let (second, earlier) =
        review(&mut log, &validator, "t1", vec![Item::user_text("go on")]).await;
    assert_eq!(earlier.gaps, vec![CoverageGap::WithheldControlTraffic]);
    calling_turn(&mut log, &second, Item::tool_call("x", "grep", "{}")).await;
    let (_, later) = review(
        &mut log,
        &validator,
        "t2",
        vec![tool_result("x", "GREP-OUTPUT")],
    )
    .await;

    assert!(later.gaps.is_empty(), "{:?}", later.gaps);
    assert_eq!(later.label, IntervalLabel::Positive);
    let brief = judge.saw(1).1;
    let shown = section(&brief).unwrap_or_else(|| panic!("no section:\n{brief}"));
    assert!(shown.contains("GREP-OUTPUT"), "{shown}");
}

/// **On the Responses surface the stored namespace decides.** `status` under
/// another server is the agent's work. Under roundhouse's namespace, or with
/// no namespace in a record written before the field existed, it is ours.
#[tokio::test]
async fn on_the_responses_surface_the_namespace_decides_whose_call_it_is() {
    for (namespace, ours) in [
        (Some("mcp__other"), false),
        (Some(CONTROL_TOOL_NAMESPACE), true),
        (None, true),
    ] {
        let mut log = Log::enrolled(Some(Arm::Shadow)).await;
        let first = log.begin("t0", vec![Item::user_text("start")]).await;
        calling_turn(
            &mut log,
            &first,
            Item::namespaced_tool_call("c1", "status", namespace.map(str::to_string), "{}"),
        )
        .await;
        let judge = ScriptedJudge::answering(&[ON_TRACK]);
        let validator = validator_over(judge.clone(), DEFAULT_INTERVAL_SECTION_BYTES);
        let (_, reviewed) = review_on(
            &mut log,
            &validator,
            "t1",
            vec![tool_result("c1", "STATUS-OUTPUT")],
            ControlCallDialect::CodexResponses,
        )
        .await;
        let brief = judge.saw(0).1;
        match ours {
            true => {
                assert_eq!(
                    reviewed.gaps,
                    vec![CoverageGap::WithheldControlTraffic],
                    "{namespace:?}"
                );
                assert_eq!(reviewed.label, IntervalLabel::Unknown, "{namespace:?}");
                assert!(!brief.contains("STATUS-OUTPUT"), "{namespace:?}:\n{brief}");
            }
            false => {
                assert!(
                    reviewed.gaps.is_empty(),
                    "{namespace:?}: {:?}",
                    reviewed.gaps
                );
                assert_eq!(reviewed.label, IntervalLabel::Positive, "{namespace:?}");
                let shown = section(&brief).unwrap_or_else(|| panic!("no section:\n{brief}"));
                assert!(shown.contains("STATUS-OUTPUT"), "{namespace:?}:\n{shown}");
            }
        }
    }
}
