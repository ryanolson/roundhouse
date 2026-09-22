// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What this module asks, and how it finds the answer again.

use super::*;

/// The question set is one entry, and it is a question about the *turn*.
///
/// The transport carries several questions in one request. This module is not
/// the rich classifier yet, and the assertion that
/// keeps it honest is the count. The options are tiers rather than model ids
/// because `validate::brief` rules that the routing decision is taken by code:
/// a criteria list of model names would ask a third party to route.
#[test]
fn the_adapter_asks_one_question_about_the_turn() {
    let questions = TypeSafeShadow::<ByteTokenizer>::questions();

    assert_eq!(questions.len(), 1, "{questions:?}");
    let tier = questions
        .get(TIER_KEY)
        .unwrap_or_else(|| panic!("the question is filed under `{TIER_KEY}`: {questions:?}"));
    assert_eq!(
        tier.criteria.keys().collect::<Vec<_>>(),
        ["capable", "efficient"],
        "the options are tiers, and a model id among them would move the \
         routing decision to the service"
    );
    for option in tier.criteria.keys() {
        assert!(
            !option.contains('/') && !option.contains('-'),
            "`{option}` reads like a model id rather than a tier"
        );
    }
}

/// The adapter asks under the id it recovers the answer under.
///
/// Two literals at the two ends would let a request go out under one id and be
/// looked up under another, which the transport reports as an unusable batch
/// rather than as the typo it is. Asserted on the body that actually left and
/// on the outcome that came back, so the round trip is the thing under test.
#[tokio::test]
async fn the_answer_is_recovered_under_the_id_it_was_asked_under() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let outcome = shadow(addr, config().enable(), ledger)
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    match &outcome {
        ShadowOutcome::Answered { answer, .. } => {
            assert_eq!(answer.choice, "capable");
            assert_eq!(answer.probabilities["efficient"], 0.15);
        }
        other => panic!("{other:?}"),
    }

    let sent: serde_json::Value = serde_json::from_str(&up.body()).expect("a JSON body arrived");
    let asked = sent["questions"]
        .as_object()
        .unwrap_or_else(|| panic!("a `questions` map: {sent}"));
    assert_eq!(
        asked.len(),
        1,
        "one question went out, so one answer is what the batch contract \
         requires back: {sent}"
    );
    assert!(asked.contains_key(TIER_KEY), "{sent}");
    assert_eq!(sent["questions"][TIER_KEY]["type"], "choice", "{sent}");
}
