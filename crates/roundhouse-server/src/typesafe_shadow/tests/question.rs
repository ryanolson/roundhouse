// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What this module asks, and how it finds the answers again.

use super::*;
use roundhouse_core::classify::{
    ClassificationAxis, ContextDependence, TierChoice, TurnComplexity, TurnIntent,
};

/// **Three questions about the turn, and none about the models.**
///
/// The tier-only question the taxonomy replaced named the *decision* rather
/// than the turn, so a later selector that wanted a different mapping had
/// nothing to re-map from. The tier question now rides beside the three axes
/// rather than instead of them; its own rubric test is below. The assertion
/// that keeps the set honest is that no option reads like a target:
/// `validate::brief` rules that the routing decision is taken by code, and a
/// criteria list of model names would ask a third party to route.
#[test]
fn the_adapter_asks_the_three_taxonomy_questions_and_names_no_model() {
    let questions = TypeSafeShadow::<ByteTokenizer>::questions();

    assert_eq!(questions.len(), 4, "three axes and the tier: {questions:?}");
    for key in [TurnIntent::KEY, TurnComplexity::KEY, ContextDependence::KEY] {
        let question = questions
            .get(key)
            .unwrap_or_else(|| panic!("a question under `{key}`: {questions:?}"));
        assert!(!question.instructions.is_empty());
        assert!(
            question.criteria.contains_key("unknown"),
            "`{key}` must let the classifier say it cannot tell"
        );
        for option in question.criteria.keys() {
            assert!(
                !option.contains('/'),
                "`{option}` reads like a provider/model pair rather than a \
                 description of the turn"
            );
        }
    }
    // Every option the taxonomy defines is offered, so a parser that accepts a
    // label the request never offered cannot exist.
    assert_eq!(
        questions[TurnIntent::KEY].criteria.len(),
        TurnIntent::OPTIONS.len()
    );
    assert_eq!(
        questions[TurnComplexity::KEY].criteria.len(),
        TurnComplexity::OPTIONS.len()
    );
    assert_eq!(
        questions[ContextDependence::KEY].criteria.len(),
        ContextDependence::OPTIONS.len()
    );
}

/// The adapter asks under the ids it recovers the answers under.
///
/// Two literals at the two ends would let a request go out under one id and be
/// looked up under another, which the transport reports as an unusable batch
/// rather than as the typo it is. Asserted on the body that actually left and
/// on the record that came back, so the round trip is the thing under test.
#[tokio::test]
async fn the_answers_are_recovered_under_the_ids_they_were_asked_under() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    let classification = record.outcome.classification().expect("a classification");
    assert_eq!(classification.intent.value, TurnIntent::Implement);
    assert_eq!(classification.intent.confidence, 0.82);
    assert_eq!(classification.complexity.value, TurnComplexity::Involved);
    assert_eq!(
        classification.context_dependence.value,
        ContextDependence::Recent
    );
    assert_eq!(
        classification.taxonomy_version,
        roundhouse_core::classify::TAXONOMY_VERSION
    );

    let sent: serde_json::Value = serde_json::from_str(&up.body()).expect("a JSON body arrived");
    let asked = sent["questions"]
        .as_object()
        .unwrap_or_else(|| panic!("a `questions` map: {sent}"));
    assert_eq!(
        asked.len(),
        4,
        "four questions went out, so four answers are what the batch \
         contract requires back: {sent}"
    );
    for key in [TurnIntent::KEY, TurnComplexity::KEY, ContextDependence::KEY] {
        assert!(asked.contains_key(key), "{sent}");
        assert_eq!(sent["questions"][key]["type"], "choice", "{sent}");
    }
    // One state for four questions, which is the saving batching buys.
    assert!(sent["state"].is_string(), "{sent}");
}

/// **The tier question rides the request the taxonomy already makes.**
///
/// One call, one state, four questions: the tier answer adds no call, no
/// deadline and no budget line (2026-09-28 addendum, "Jev as a scout", item 1).
/// Asserted on the body that actually left and on the record that came back.
#[tokio::test]
async fn the_tier_question_rides_the_same_request_as_the_taxonomy() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1, "one request carries every question");
    assert_eq!(ledger.requested().len(), 1, "one hold for all four answers");
    let sent: serde_json::Value = serde_json::from_str(&up.body()).expect("a JSON body arrived");
    let asked = sent["questions"]
        .as_object()
        .unwrap_or_else(|| panic!("a `questions` map: {sent}"));
    assert_eq!(asked.len(), 4, "the taxonomy and the tier question: {sent}");
    let tier = &sent["questions"][TierChoice::KEY];
    assert_eq!(tier["type"], "choice", "{sent}");
    let offered: Vec<&str> = tier["criteria"]
        .as_object()
        .unwrap_or_else(|| panic!("tier criteria: {sent}"))
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(offered, ["capable", "efficient"], "{sent}");

    let classification = record.outcome.classification().expect("a classification");
    assert_eq!(classification.taxonomy_version, 2);
    let tier = classification.tier.expect("the tier answer is recorded");
    assert_eq!(tier.value, TierChoice::Capable);
    assert_eq!(tier.confidence, 0.7, "the service's confidence is kept");
}

/// **The tier options describe the work and name no model and no price.**
///
/// The 2026-09-17 ruling, section 2, allowed a tier question only when its
/// criteria describe the work. A rubric that named a model would ask a third
/// party to route; one that named a price would make it a cost question.
#[test]
fn a_tier_option_rubric_names_no_model_and_no_price() {
    let questions = TypeSafeShadow::<ByteTokenizer>::questions();
    let tier = questions
        .get(TierChoice::KEY)
        .unwrap_or_else(|| panic!("a question under `{}`: {questions:?}", TierChoice::KEY));
    let labels: Vec<&str> = tier.criteria.keys().map(String::as_str).collect();
    assert_eq!(labels, ["capable", "efficient"]);

    const FORBIDDEN: &[&str] = &[
        // Vendors and model families a deployment is likely to serve.
        "anthropic",
        "claude",
        "opus",
        "sonnet",
        "haiku",
        "openai",
        "gpt",
        "gemini",
        "llama",
        "qwen",
        "kimi",
        "deepseek",
        "mistral",
        "jev",
        "local",
        "model",
        // Anything priced.
        "$",
        "usd",
        "price",
        "cost",
        "cheap",
        "expensive",
        "dollar",
        "token",
        "budget",
    ];
    let texts = std::iter::once(("instructions", tier.instructions.as_str())).chain(
        tier.criteria
            .iter()
            .map(|(label, rubric)| (label.as_str(), rubric.as_str())),
    );
    for (what, text) in texts {
        assert!(!text.trim().is_empty(), "`{what}` has text");
        let lower = text.to_lowercase();
        for word in FORBIDDEN {
            assert!(
                !lower.contains(word),
                "`{what}` names `{word}`, which is about a model or a price \
                 rather than the work: {text}"
            );
        }
        assert!(
            !text.contains('/'),
            "`{what}` reads like a provider/model pair: {text}"
        );
    }
}

/// **Under taxonomy 2 a reply without the tier answer is unusable.**
///
/// The existing complete-set rule, applied to the fourth question: three
/// answers to four questions is not a weaker classification but a different
/// one, and it is recorded as unusable with its accounting intact.
#[tokio::test]
async fn a_reply_without_the_tier_answer_is_unusable_under_taxonomy_2() {
    let (addr, _up) = upstream(ANSWER_WITHOUT_TIER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    match &record.outcome {
        ClassificationOutcome::Unusable { reason, spend, .. } => {
            assert_eq!(reason, "missing_answer");
            assert!(
                matches!(spend, EvaluationSpend::Measured { .. }),
                "the usage the service reported survives: {spend:?}"
            );
        }
        other => panic!("three answers to four questions is unusable: {other:?}"),
    }
    assert_eq!(roundhouse_core::classify::TAXONOMY_VERSION, 2);
}
