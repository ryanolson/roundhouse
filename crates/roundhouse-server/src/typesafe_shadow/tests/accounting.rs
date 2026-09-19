// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

/// A grant of less than the estimate is a refusal, not a smaller call: the
/// prompt is already written and its price is not negotiable downwards.
///
/// A zero grant is an ordinary ledger answer rather than an error, so a
/// refusal spelled as "the call failed" would send this request unfunded.
#[tokio::test]
async fn a_short_or_zero_grant_makes_no_call_and_hands_the_hold_back() {
    for (why, granted) in [("a zero grant", 0.0), ("a short grant", 0.000_000_1)] {
        let (addr, up) = upstream(ANSWER).await;
        let ledger = RecordingLedger::granting(granted);
        let credential = credential();
        let pool = Pool::of(vec![frontier()]);

        let outcome = shadow(addr, config().enable(), ledger.clone())
            .classify(
                call(&credential),
                &items(),
                Objective::Unknown,
                Vec::new(),
                &pool.admitted(),
            )
            .await;

        assert_eq!(
            outcome,
            ShadowOutcome::NotRun(NotRun::BudgetRefused),
            "{why}"
        );
        assert_eq!(up.count(), 0, "{why}: refused before a socket");
        assert_eq!(
            ledger.settled(),
            vec![0.0],
            "{why}: the partial reservation buys nothing and is handed straight \
             back, or it strands the experiment's money for a TTL"
        );
    }
}

/// A ledger nobody can reach fails closed.
#[tokio::test]
async fn an_unavailable_ledger_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::unavailable();
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let outcome = shadow(addr, config().enable(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    assert_eq!(outcome, ShadowOutcome::NotRun(NotRun::LedgerUnavailable));
    assert_eq!(
        up.count(),
        0,
        "spending against a ceiling nobody could confirm is an unbudgeted call"
    );
}

/// A billed call whose answer is unusable still settles what it cost.
#[tokio::test]
async fn an_unusable_answer_settles_the_usage_that_was_reported() {
    let (addr, _up) = upstream(ANSWER_UNUSABLE).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let outcome = shadow(addr, config().enable(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    let expected = (312.0 * 1.0 + 48.0 * 2.0) / 1_000_000.0;
    assert_eq!(
        outcome,
        ShadowOutcome::Unusable {
            signal: SignalError::SumIsNotOne,
            accounting: Accounting::Measured {
                usage: SystemOneUsage {
                    input_tokens: 312,
                    output_tokens: 48
                },
                usd: expected,
            },
        }
    );
    assert_eq!(
        ledger.settled(),
        vec![expected],
        "a call that was billed for must not settle at zero because its answer \
         was unusable"
    );
}

/// Accounting that did not fully arrive is unknown. The hold is still released
/// at zero — that is how a hold is closed — but nothing reports a free call,
/// and the signal that did arrive survives.
#[tokio::test]
async fn unreported_usage_releases_the_hold_without_claiming_a_free_call() {
    let (addr, up) = upstream(ANSWER_PARTIAL_USAGE).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let outcome = shadow(addr, config().enable(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    assert_eq!(up.count(), 1);
    match &outcome {
        ShadowOutcome::Answered { answer, accounting } => {
            assert_eq!(
                answer.choice, "capable",
                "an unpriceable call still answered"
            );
            assert_eq!(
                *accounting,
                Accounting::Unknown,
                "a half-reported spend is unknown accounting; a measured zero \
                 here books a billed call as free"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        ledger.settled(),
        vec![0.0],
        "the hold is released, which is not the same statement as the call \
         having been free"
    );
}

/// A call that produced no envelope carries no accounting at all.
#[tokio::test]
async fn a_failed_call_releases_its_hold_with_no_accounting() {
    let (addr, _up) = upstream("<html>502</html>").await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let outcome = shadow(addr, config().enable(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    assert_eq!(
        outcome,
        ShadowOutcome::Failed {
            error: SystemOneError::Malformed
        }
    );
    assert_eq!(ledger.settled(), vec![0.0]);
}
