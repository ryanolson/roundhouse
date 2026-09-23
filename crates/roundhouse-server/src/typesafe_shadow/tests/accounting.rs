// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::test_support::captured_warnings;

/// A grant of less than the quote is a refusal, not a smaller call: the prompt
/// is already written and its price is not negotiable downwards.
///
/// A zero grant is an ordinary ledger answer rather than an error, so a refusal
/// spelled as "the call failed" would send this request unfunded.
///
/// **A durable outcome rather than a `NotRun`.** The grant happens on the worker
/// now, so by the time a ledger can refuse anything the intent is already
/// written — and a refusal that left no result would read like a process that
/// died, which is the one state it is not.
#[tokio::test]
async fn a_short_or_zero_grant_makes_no_call_and_says_so_durably() {
    for (why, granted) in [("a zero grant", 0.0), ("a short grant", 0.000_000_1)] {
        let (addr, up) = upstream(ANSWER).await;
        let ledger = RecordingLedger::granting(granted);
        let credential = credential();

        let record = classify(
            &shadow(addr, config(), ledger.clone()),
            &credential,
            Some(&[frontier()]),
        )
        .await
        .expect("prepared");

        match record.outcome {
            ClassificationOutcome::Unfunded {
                reason: FundingRefusal::BudgetRefused { granted_usd, .. },
            } => assert_eq!(granted_usd, granted, "{why}"),
            other => panic!("{why}: {other:?}"),
        }
        assert_eq!(up.count(), 0, "{why}: refused before a socket");
        assert_eq!(
            ledger.settled(),
            vec![0.0],
            "{why}: the partial reservation buys nothing and is handed straight \
             back, or it strands the experiment's money for a TTL"
        );
        assert!(
            record.outcome.spend().is_none(),
            "{why}: nothing was sent, which is not the same as an unknown cost"
        );
    }
}

/// A ledger nobody can reach fails closed, and records that it did.
#[tokio::test]
async fn an_unavailable_ledger_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::unavailable();
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(
        record.outcome,
        ClassificationOutcome::Unfunded {
            reason: FundingRefusal::LedgerUnavailable,
        }
    );
    assert_eq!(
        up.count(),
        0,
        "spending against a ceiling nobody could confirm is an unbudgeted call"
    );
}

/// **The quote is taken over the exact bytes that go on the wire.**
///
/// Three serializations would be three chances for the thing measured to differ
/// from the thing sent, and a hold taken against one body while another goes on
/// the wire is a budget that authorized spend it never approved. Asserted in
/// tokens rather than in dollars, because that comparison is exact.
#[tokio::test]
async fn the_hold_is_asked_for_what_the_sent_bytes_are_worth() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let shadow = shadow(addr, config(), ledger.clone());

    let projection = shadow.projection(&capture(), &[], &[]).unwrap();
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    let reservation = prepared.intent.reservation.clone();
    let record = shadow.execute(prepared, never()).await;

    assert_eq!(up.count(), 1);
    // `ByteTokenizer` is one token per byte, so the arrived body's length is
    // exactly what the quote must have been taken over.
    assert_eq!(reservation.estimated_input_tokens as usize, up.body().len());
    assert_eq!(
        ledger.requested(),
        vec![reservation.requested_usd],
        "the hold is asked for the quote the record carries"
    );
    assert!(
        (reservation.requested_usd - quote_usd(up.body().len())).abs() < f64::EPSILON,
        "and that quote prices both axes at the configured rates: {} against {}",
        reservation.requested_usd,
        quote_usd(up.body().len())
    );
    match &record.outcome {
        ClassificationOutcome::Classified { .. } => {}
        other => panic!("{other:?}"),
    }
}

/// A billed call whose answer is unusable still settles what it cost, and
/// records no classification.
#[tokio::test]
async fn an_unusable_answer_settles_the_usage_that_was_reported() {
    let (addr, _up) = upstream(ANSWER_UNUSABLE).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(
        record.outcome,
        ClassificationOutcome::Unusable {
            reason: "sum_is_not_one".to_string(),
            spend: EvaluationSpend::Measured {
                usage: EvaluationUsage {
                    input_tokens: 312,
                    output_tokens: 48,
                },
                usd: REPORTED_USD,
                granted_usd: 1_000.0,
                settled: SettlementAck::Committed,
            },
            // An answer set this deployment cannot use still came from
            // somewhere, and which build produced it is what the unusable
            // answer gets investigated with.
            reported_model: Some("jev-1.12".to_string()),
        }
    );
    assert_eq!(
        ledger.settled(),
        vec![REPORTED_USD],
        "a call that was billed for must not settle at zero because its answer \
         was unusable"
    );
}

/// **A partial taxonomy is not a weaker classification.**
///
/// Two of three axes answered is indistinguishable from a record whose third
/// axis is `unknown` if the partial set is kept, and those are exactly the two
/// states this vocabulary exists to hold apart.
#[tokio::test]
async fn a_missing_axis_supplies_no_classification_at_all() {
    let (addr, up) = upstream(ANSWER_PARTIAL_TAXONOMY).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1);
    assert!(
        record.outcome.classification().is_none(),
        "two axes out of three is not a classification: {:?}",
        record.outcome
    );
    assert_eq!(
        record.outcome.committed_usd(),
        Some(REPORTED_USD),
        "and its accounting survives, which is the whole reason usage sits \
         outside the answers"
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

    let record = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(up.count(), 1);
    match &record.outcome {
        ClassificationOutcome::Classified {
            classification,
            spend,
            reported_model,
        } => {
            assert_eq!(
                classification.intent.value,
                roundhouse_core::classify::TurnIntent::Implement,
                "an unpriceable call still answered"
            );
            assert_eq!(
                reported_model.as_deref(),
                Some("jev-1.12"),
                "and it said which model did, which is a fact about the call \
                 and not about whether its cost could be established"
            );
            assert_eq!(
                *spend,
                EvaluationSpend::Unknown {
                    granted_usd: 1_000.0,
                    settled: SettlementAck::Committed
                },
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

/// **A settle the ledger refused is not a committed charge.**
///
/// The interface this replaced said only that a spend had been *submitted*, so
/// every consumer read a rejected settle as money committed — a ledger outage
/// would have read downstream as a discount. The usage is still reported,
/// because the service still billed it.
///
/// A blocking test with its own current-thread runtime, because `with_default`
/// installs a *thread-local* subscriber and a multi-threaded runtime is free to
/// resume the future on a thread that never had one.
#[test]
fn a_rejected_settle_is_recorded_as_rejected_and_names_the_call() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (addr, up) = rt.block_on(upstream(ANSWER));
    let ledger = RecordingLedger::granting_but_unsettleable(1_000.0);
    let credential = credential();

    let mut record = None;
    let warned = captured_warnings(|| {
        record = Some(
            rt.block_on(classify(
                &shadow(addr, config(), ledger.clone()),
                &credential,
                Some(&[frontier()]),
            ))
            .expect("prepared"),
        );
    });
    let record = record.expect("a record");

    // The preconditions the acknowledgement is about: a call happened, it was
    // priced, and the failed commit changed neither the answer nor what was
    // submitted.
    assert_eq!(up.count(), 1);
    assert_eq!(ledger.settled(), vec![REPORTED_USD]);
    let spend = record.outcome.spend().expect("a call was made");
    assert_eq!(
        *spend,
        EvaluationSpend::Measured {
            usage: EvaluationUsage {
                input_tokens: 312,
                output_tokens: 48,
            },
            usd: REPORTED_USD,
            granted_usd: 1_000.0,
            settled: SettlementAck::Unconfirmed,
        },
        "reported usage and a committed charge are two facts"
    );
    assert_eq!(
        spend.committed_usd(),
        None,
        "nothing this deployment can point at says the charge landed"
    );
    assert!(
        record.outcome.classification().is_some(),
        "a settle that cannot be applied is a warning and a skip, so the answer \
         survives it"
    );

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
        "and the call it was taken under, which is what an operator \
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

/// A call that produced no envelope has unknown accounting and a reason that
/// names the failure class without quoting the service.
#[tokio::test]
async fn a_failed_call_releases_its_hold_with_unknown_accounting() {
    let (addr, _up) = upstream("<html>502</html>").await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await
    .expect("prepared");

    assert_eq!(
        record.outcome,
        ClassificationOutcome::Failed {
            reason: "malformed".to_string(),
            spend: EvaluationSpend::Unknown {
                granted_usd: 1_000.0,
                settled: SettlementAck::Committed,
            },
        }
    );
    assert_eq!(ledger.settled(), vec![0.0]);
}

/// **A prepared call that is never executed reserves nothing.**
///
/// The engine's path when the durable intent cannot be committed. There is no
/// hold to hand back, because opening one is exactly what preparing no longer
/// does — which is the whole of "an unreachable evaluation ledger cannot delay
/// or fail a turn".
#[tokio::test]
async fn a_prepared_call_that_is_dropped_touches_no_ledger() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let shadow = shadow(addr, config(), ledger.clone());

    let projection = shadow.projection(&capture(), &[], &[]).unwrap();
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    drop(prepared);

    assert_eq!(up.count(), 0, "nothing was sent");
    assert!(
        ledger.requested().is_empty() && ledger.settled().is_empty(),
        "and the evaluation ledger was never asked anything"
    );
}

/// **The durable intent carries the terms, not a digest of them.**
///
/// Later accounting has to answer "what was this allowed to cost, and against
/// which ceiling" from the log alone, long after the file that produced those
/// numbers was edited.
#[tokio::test]
async fn the_intent_records_the_reservation_in_numbers() {
    let (addr, _up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let shadow = shadow(addr, config(), ledger.clone());

    let projection = shadow.projection(&capture(), &[], &[]).unwrap();
    let prepared = shadow
        .prepare(call(&credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    let intent = &prepared.intent;

    assert_eq!(intent.call_id, ResponseId::new("shadow_1"));
    assert_eq!(intent.source_turn_index, 3);
    assert_eq!(intent.source_response_id, ResponseId::new("resp_3"));
    assert_eq!(intent.expires_at_ms, EXPIRES_MS);
    assert_eq!(intent.identity.model, "jev-1.12");
    assert_eq!(intent.identity.config_revision, CONFIG_REVISION);
    assert_eq!(
        intent.identity.taxonomy_version,
        roundhouse_core::classify::TAXONOMY_VERSION
    );

    let reservation = &intent.reservation;
    assert_eq!(reservation.rate_card, pricing());
    assert_eq!(reservation.expected_output_tokens, EXPECTED_OUTPUT_TOKENS);
    assert!(
        reservation.requested_usd > 0.0,
        "the quote the hold will be asked for"
    );
    assert!(
        ledger.requested().is_empty(),
        "and no ledger has been asked anything yet: an intent records a quote, \
         never a grant"
    );
    assert_eq!(reservation.budget_limit_usd, 1_000.0);
    assert_eq!(reservation.budget_window, BudgetWindow::Total);
    assert_eq!(
        reservation.member_ceiling_usd, None,
        "this membership pools the project's ceiling"
    );
    assert_eq!(reservation.warn_at, 0.8);
    assert!(
        reservation.hold_ttl_ms > EXPIRES_MS - NOW_MS,
        "the hold outlives the call's own expiry, or a queued call settles \
         against a hold that lapsed while it waited"
    );
}
