// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What this module asks, and how it finds the answers again.

use super::*;
use roundhouse_core::classify::{
    ClassificationAxis, ContextDependence, TurnComplexity, TurnIntent,
};

/// **Three questions about the turn, and none about the models.**
///
/// The tier question this replaced named the *decision* rather than the turn,
/// so a later selector that wanted a different mapping had nothing to re-map
/// from. The assertion that keeps the new set honest is that no option reads
/// like a target: `validate::brief` rules that the routing decision is taken by
/// code, and a criteria list of model names would ask a third party to route.
#[test]
fn the_adapter_asks_the_three_taxonomy_questions_and_names_no_model() {
    let questions = TypeSafeShadow::<ByteTokenizer>::questions();

    assert_eq!(questions.len(), 3, "{questions:?}");
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
        3,
        "three questions went out, so three answers are what the batch \
         contract requires back: {sent}"
    );
    for key in [TurnIntent::KEY, TurnComplexity::KEY, ContextDependence::KEY] {
        assert!(asked.contains_key(key), "{sent}");
        assert_eq!(sent["questions"][key]["type"], "choice", "{sent}");
    }
    // One state for three questions, which is the saving batching buys.
    assert!(sent["state"].is_string(), "{sent}");
}
