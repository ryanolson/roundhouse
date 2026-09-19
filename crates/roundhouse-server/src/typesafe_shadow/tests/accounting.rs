// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::test_support::captured_warnings;

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

/// A settle that could not be applied says **which call** it left uncommitted.
///
/// This warning is the whole of the record: the call was made, it was priced,
/// and the ledger then refused the commit — and with B2's durable allocation
/// record still unwired, nothing else in this process writes the call down. A
/// line that carries only the ledger's own error message tells an operator that
/// *some* shadow evaluation is unaccounted for and gives them no way to say
/// which session or which hold, which is the same as not warning at all in a
/// deployment making more than one of these calls.
///
/// Driven through `classify` rather than by calling `settle` directly, so the
/// fields are asserted on the identity a real call actually carries.
///
/// A blocking test with its own current-thread runtime, because `with_default`
/// installs a *thread-local* subscriber and a multi-threaded runtime is free to
/// resume the future on a thread that never had one.
#[test]
fn a_settle_that_cannot_be_applied_names_the_call_it_left_uncommitted() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (addr, up) = rt.block_on(upstream(ANSWER));
    let ledger = RecordingLedger::granting_but_unsettleable(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let mut outcome = None;
    let warned = captured_warnings(|| {
        outcome = Some(
            rt.block_on(shadow(addr, config().enable(), ledger.clone()).classify(
                call(&credential),
                &items(),
                Objective::Unknown,
                Vec::new(),
                &pool.admitted(),
            )),
        );
    });

    // The preconditions the warning is about: a call happened, it was priced,
    // and the failed commit changed neither the outcome nor what was submitted.
    let priced = (312.0 * 1.0 + 48.0 * 2.0) / 1_000_000.0;
    assert_eq!(up.count(), 1);
    assert!(
        matches!(
            outcome,
            Some(ShadowOutcome::Answered {
                accounting: Accounting::Measured { usd, .. },
                ..
            }) if usd == priced
        ),
        "a settle that cannot be applied is a warning and a skip, so the answer \
         and its accounting survive it: {outcome:?}"
    );
    assert_eq!(ledger.settled(), vec![priced]);

    assert!(
        warned.contains("could not be committed"),
        "the settle failure must be warned about at all:\n{warned}"
    );
    assert!(
        warned.contains("sess_shadow"),
        "the warning must name the session whose spend is uncommitted:\n{warned}"
    );
    assert!(
        warned.contains("shadow_1"),
        "and the hold key it was taken under, which is what an operator \
         reconciles against:\n{warned}"
    );
    // The other half of the same rule: this line goes to a log, so it carries
    // identity and nothing else. A transcript or a key in a warning is the
    // egress this whole module is gated to prevent.
    assert!(
        !warned.contains("trailing commas") && !warned.contains("Rust repository"),
        "the transcript must not reach the log:\n{warned}"
    );
    assert!(!warned.contains(KEY), "nor the deployment's key:\n{warned}");
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
