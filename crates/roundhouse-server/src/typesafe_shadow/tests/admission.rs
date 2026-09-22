// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use roundhouse_core::control::PresentedCredential;

/// Opt-in, and the type is what enforces it.
#[test]
fn a_shadow_config_is_disabled_until_a_deployment_says_otherwise() {
    assert!(
        !config().is_enabled(),
        "a deployment that named a model and a rate card has still not agreed \
         to send anybody's prompt to a third party"
    );
    assert!(config().enable().is_enabled());
}

/// The default config makes no call at all.
#[tokio::test]
async fn a_disabled_shadow_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let outcome = classify(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        Some(&[frontier()]),
    )
    .await;

    assert_eq!(outcome.err(), Some(NotRun::Disabled));
    assert_eq!(up.count(), 0);
    assert!(
        ledger.settled().is_empty(),
        "nothing was held, so nothing is released"
    );
}

/// A session whose policy admitted no frontier target has no third party its
/// content is already permitted to reach, so it makes zero calls.
#[tokio::test]
async fn a_local_only_session_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let outcome = classify(
        &shadow(addr, config().enable(), ledger.clone()),
        &credential,
        Some(&[local()]),
    )
    .await;

    assert_eq!(outcome.err(), Some(NotRun::NoAdmittedFrontier));
    assert_eq!(
        up.count(),
        0,
        "a local-only session must not egress to a hosted classifier"
    );
    assert!(ledger.settled().is_empty());
}

/// **A decision with no admission evidence cannot grant permission.**
///
/// A policy outside the builtin set can assemble its own `Decision` and leave
/// `admitted` as `None` — the routing snapshot records that as unknown rather
/// than as a nearest fit. Resolving admission here instead would ask a
/// *different* question, with a guessed load ceiling and without the overflow
/// valve, and then record its answer as this decision's consent to send a
/// tenant's prompt to a third party.
#[tokio::test]
async fn a_decision_without_admission_evidence_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let outcome = classify(
        &shadow(addr, config().enable(), ledger.clone()),
        &credential,
        None,
    )
    .await;

    assert_eq!(outcome.err(), Some(NotRun::AdmissionUnknown));
    assert_eq!(up.count(), 0, "silence is not consent");
    assert!(
        ledger.requested().is_empty(),
        "and no hold was opened for it"
    );
}

/// An empty admitted pool is the same refusal as a local-only one: admission
/// left nothing, so nothing is permitted.
#[tokio::test]
async fn an_empty_admitted_pool_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let outcome = classify(
        &shadow(addr, config().enable(), ledger.clone()),
        &credential,
        Some(&[]),
    )
    .await;

    assert_eq!(outcome.err(), Some(NotRun::NoAdmittedFrontier));
    assert_eq!(up.count(), 0);
}

/// The control for the refusals above: an enabled deployment whose policy
/// admitted a frontier target does call, so none is passing by never calling.
#[tokio::test]
async fn an_enabled_shadow_with_an_admitted_frontier_target_calls() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();

    let record = classify(
        &shadow(addr, config().enable(), ledger.clone()),
        &credential,
        Some(&[local(), frontier()]),
    )
    .await
    .expect("prepared");

    let classification = record
        .outcome
        .classification()
        .expect("a complete answer set");
    assert_eq!(
        classification.intent.value,
        roundhouse_core::classify::TurnIntent::Implement
    );
    assert_eq!(
        classification.complexity.value,
        roundhouse_core::classify::TurnComplexity::Involved
    );
    assert_eq!(
        classification.context_dependence.value,
        roundhouse_core::classify::ContextDependence::Recent
    );
    assert_eq!(up.count(), 1);
}

/// A credential this module may not spend is an eligibility failure, decided
/// before any hold is opened.
///
/// A forwarded seat is the tenant's own and is not roundhouse's to offer a
/// third party; no credential at all is a local refusal. Neither is a *failed
/// call*: nothing reached a socket, so reporting one would give a caller two
/// typed answers for one condition. The settle assertion is what separates
/// "refused before the grant" from "refused after it, hold handed back" —
/// charging the experiment's ledger a round trip for a call that was never
/// eligible is the thing being prevented.
#[tokio::test]
async fn a_credential_this_module_may_not_spend_takes_no_hold() {
    let forwarded = TurnCredential::Forwarded(
        PresentedCredential::captured(|name| match name {
            "authorization" => Some("Bearer tenant-seat-ZZZQQQ".to_string()),
            _ => None,
        })
        .expect("a bearer was presented")
        .for_provider("anthropic")
        .expect("anthropic has an allowlist row"),
    );
    for (why, credential) in [
        ("a forwarded tenant seat", forwarded),
        ("no credential at all", TurnCredential::Absent),
    ] {
        let (addr, up) = upstream(ANSWER).await;
        let ledger = RecordingLedger::granting(1_000.0);

        let outcome = classify(
            &shadow(addr, config().enable(), ledger.clone()),
            &credential,
            Some(&[frontier()]),
        )
        .await;

        assert!(
            matches!(outcome.as_ref().err(), Some(NotRun::Refused(_))),
            "{why}: an ineligible credential is a refusal, not a failed call: \
             {:?}",
            outcome.err()
        );
        assert_eq!(up.count(), 0, "{why}");
        assert!(
            ledger.settled().is_empty(),
            "{why}: no hold may be opened for a call that was never eligible"
        );
    }
}
