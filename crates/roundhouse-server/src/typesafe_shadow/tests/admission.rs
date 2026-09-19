// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

/// Opt-in, and the type is what enforces it.
#[test]
fn a_shadow_config_is_disabled_until_a_deployment_says_otherwise() {
    assert!(
        !config().is_enabled(),
        "a deployment that named a model and a rate card has still not agreed \
         to send anybody's transcript to a third party"
    );
    assert!(config().enable().is_enabled());
}

/// The default config makes no call at all.
#[tokio::test]
async fn a_disabled_shadow_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![frontier()]);

    let outcome = shadow(addr, config(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    assert_eq!(outcome, ShadowOutcome::NotRun(NotRun::Disabled));
    assert_eq!(up.count(), 0);
    assert!(
        ledger.settled().is_empty(),
        "nothing was held, so nothing is released"
    );
}

/// A session with no admitted frontier target has no third party its content
/// is already permitted to reach, so it makes zero calls.
#[tokio::test]
async fn a_local_only_session_makes_no_call() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![local()]);

    let outcome = shadow(addr, config().enable(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    assert_eq!(outcome, ShadowOutcome::NotRun(NotRun::NoAdmittedFrontier));
    assert_eq!(
        up.count(),
        0,
        "a local-only session must not egress to a hosted classifier"
    );
    assert!(ledger.settled().is_empty());
}

/// Four ways admission removes the frontier while the pool still holds one.
///
/// This is the case the design turns on, and a catalog- or pool-level check
/// would pass every one of them: the frontier candidate is present throughout,
/// and only `RoutingContext::admissible` knows it is unreachable this turn.
/// The allow filter and the quality floor are reachability; the cadence and the
/// budget are this-turn axes. All four must make zero calls.
#[tokio::test]
async fn a_frontier_target_admission_removes_makes_no_call() {
    let cases: Vec<(&str, Pool)> = vec![
        (
            "an allow filter that names only the local model",
            Pool::of(vec![local(), frontier()]).under(TurnPolicy {
                allow: TargetFilter::parse(["local/*"]).expect("a filter"),
                ..TurnPolicy::unrestricted()
            }),
        ),
        (
            "a quality floor above the frontier candidate's prior",
            Pool::of(vec![local(), high_quality_local(), dim_frontier()]).under(TurnPolicy {
                min_quality: 0.7,
                ..TurnPolicy::unrestricted()
            }),
        ),
        (
            // Written spent rather than spent for real: `FrontierHistory::record`
            // is crate-private because the only truthful producer is the session
            // projection. This is the admitted set a used-up ration leaves, and
            // it is how `routing/stage.rs` expresses the same state.
            "a cadence with no frontier dispatches left",
            Pool::of(vec![local(), frontier()]).under(TurnPolicy {
                frontier_cadence: Some(FrontierCadence {
                    max_frontier: 0,
                    per_turns: 10,
                }),
                ..TurnPolicy::unrestricted()
            }),
        ),
        (
            "a budget ceiling the frontier candidate does not fit under",
            Pool::of(vec![local(), frontier()]).with_budget(TurnBudget::Granted {
                ceiling_usd: 0.005,
                state: BudgetState::Exhausted,
                // Overflow disarmed, or the valve re-admits the very candidate
                // the ceiling excluded and the test measures the valve instead.
                on_exhaustion: Exhaustion::DegradeToLocal {
                    overflow_when_local_saturated: false,
                },
            }),
        ),
    ];

    for (why, pool) in cases {
        let admitted = pool.admitted();
        // The control that makes each case about *admission*: the pool really
        // does still carry a frontier target, so a check over the catalog or
        // the candidate list would have called.
        assert!(
            pool.candidates
                .iter()
                .any(|candidate| !candidate.target.is_local()),
            "{why}: the fixture must still contain a frontier candidate"
        );
        assert!(
            admitted
                .pool()
                .iter()
                .all(|candidate| candidate.target.is_local()),
            "{why}: admission was expected to remove it"
        );

        let (addr, up) = upstream(ANSWER).await;
        let ledger = RecordingLedger::granting(1_000.0);
        let credential = credential();
        let outcome = shadow(addr, config().enable(), ledger.clone())
            .classify(
                call(&credential),
                &items(),
                Objective::Unknown,
                Vec::new(),
                &admitted,
            )
            .await;

        assert_eq!(
            outcome,
            ShadowOutcome::NotRun(NotRun::NoAdmittedFrontier),
            "{why}"
        );
        assert_eq!(up.count(), 0, "{why}");
        assert!(
            ledger.requested().is_empty(),
            "{why}: and no hold was opened"
        );
    }
}

/// The control for the refusals above: an enabled deployment with an
/// admitted frontier target does call, so none is passing by never calling.
#[tokio::test]
async fn an_enabled_shadow_with_an_admitted_frontier_target_calls() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(1_000.0);
    let credential = credential();
    let pool = Pool::of(vec![local(), frontier()]);

    let outcome = shadow(addr, config().enable(), ledger.clone())
        .classify(
            call(&credential),
            &items(),
            Objective::Unknown,
            Vec::new(),
            &pool.admitted(),
        )
        .await;

    assert!(
        matches!(&outcome, ShadowOutcome::Answered { answer, .. } if answer.choice == "capable"),
        "{outcome:?}"
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

        assert!(
            matches!(outcome, ShadowOutcome::NotRun(NotRun::Refused(_))),
            "{why}: an ineligible credential is a refusal, not a failed call: \
             {outcome:?}"
        );
        assert_eq!(up.count(), 0, "{why}");
        assert!(
            ledger.settled().is_empty(),
            "{why}: no hold may be opened for a call that was never eligible"
        );
    }
}
