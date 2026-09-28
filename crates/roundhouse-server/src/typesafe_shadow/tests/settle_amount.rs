// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a call that produced no priceable reply settles at, one test per way
//! it can end.
//!
//! The rule (2026-09-28 ruling 3 in
//! `agent-docs/synergies/typesafe-selector-and-cache-affinity.md`): a request
//! that may have reached the service books the grant's estimate, and only one
//! that provably never left settles at zero. The record still says the usage
//! is unknown either way. Every assertion is on the amount the ledger was
//! asked to settle, because that is the number the budget and a later repair
//! both read.

use tokio::io::AsyncReadExt;

use super::*;

/// What the ledger grants in this file. Realistic rather than the rest of the
/// suite's 1 000 dollars, and well above any quote the fixtures here produce,
/// so a settle at the estimate is visibly neither the quote nor zero.
const GRANT: f64 = 0.05;

/// A shadow over `base` rather than over a bound upstream's address, so a
/// case can point it at a port with no listener behind it.
fn shadow_at(base: String, ledger: Arc<RecordingLedger>) -> TypeSafeShadow<ByteTokenizer> {
    let client = SystemOneClient::new(base, limits()).unwrap();
    TypeSafeShadow::new(client, config(), ledger, ByteTokenizer)
}

/// Project, prepare and execute under an explicit deadline.
async fn classify_by(
    shadow: &TypeSafeShadow<ByteTokenizer>,
    credential: &TurnCredential,
    deadline: tokio::time::Instant,
) -> ClassificationRecord {
    let projection = shadow.projection(&capture(), &[], &[]).unwrap();
    let prepared = shadow
        .prepare(call(credential), &projection, Some(&[frontier()]))
        .expect("prepared");
    shadow.execute(prepared, deadline).await
}

/// The `Failed` arm's reason and spend, or a panic naming what arrived.
fn failed(record: &ClassificationRecord) -> (&str, &EvaluationSpend) {
    match &record.outcome {
        ClassificationOutcome::Failed { reason, spend } => (reason.as_str(), spend),
        other => panic!("expected a failed call, got {other:?}"),
    }
}

/// An upstream that answers every request with `status` and an error body.
async fn status_upstream(status: u16) -> SocketAddr {
    let app = Router::new().route(
        "/systemone",
        post(move || async move {
            Response::builder()
                .status(status)
                .body(Body::from(r#"{"error":"overloaded"}"#))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// An upstream that receives the request and never answers.
async fn hanging_upstream() -> SocketAddr {
    let app = Router::new().route(
        "/systemone",
        post(|| async { std::future::pending::<Response>().await }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// **Row 1: a connection that was never established settles at zero.**
///
/// The listener is bound and dropped, so the port is known to refuse. No byte
/// of the request existed on a wire, so nothing was billed and booking the
/// estimate would over-count a call that never happened.
#[tokio::test]
async fn a_refused_connection_settles_at_zero() {
    let addr = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow_at(format!("http://{addr}"), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    let (reason, spend) = failed(&record);
    assert_eq!(reason, "transport", "{record:?}");
    assert_eq!(ledger.settled(), vec![0.0]);
    assert_eq!(
        *spend,
        EvaluationSpend::Unknown {
            granted_usd: GRANT,
            settled: SettlementAck::Committed,
            submitted_usd: 0.0,
        }
    );
}

/// **Row 2: an error status settles at zero.** The service answered and said
/// it did not do the work; the four documented codes are refusals, not
/// billed answers.
#[tokio::test]
async fn an_error_status_settles_at_zero() {
    let addr = status_upstream(529).await;
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow_at(format!("http://{addr}"), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    let (reason, spend) = failed(&record);
    assert_eq!(reason, "status");
    assert_eq!(ledger.settled(), vec![0.0]);
    assert_eq!(spend.submitted_usd(), 0.0);
}

/// **Row 3a: a connection dropped after the request went out books the
/// estimate.** The upstream read the request, so the service may have billed
/// it; settling at zero would make the evaluation arm look cheaper than it is.
#[tokio::test]
async fn a_connection_dropped_after_the_request_books_the_estimate() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 64 * 1024];
        let _ = socket.read(&mut buffer).await;
        drop(socket);
    });
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow_at(format!("http://{addr}"), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    let (reason, spend) = failed(&record);
    assert_eq!(reason, "transport", "{record:?}");
    assert_eq!(
        ledger.settled(),
        vec![GRANT],
        "a request the service received may have been billed"
    );
    assert_eq!(
        *spend,
        EvaluationSpend::Unknown {
            granted_usd: GRANT,
            settled: SettlementAck::Committed,
            submitted_usd: GRANT,
        },
        "the usage is still unknown, and the record carries what was booked"
    );
}

/// **Row 3b: the call's own deadline books the estimate.** The upstream has
/// the request and never answers; the call is cut off by `execute`'s outer
/// bound, which is exactly the shape a slow billed answer has.
#[tokio::test]
async fn a_call_cut_off_by_its_deadline_books_the_estimate() {
    let addr = hanging_upstream().await;
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow_at(format!("http://{addr}"), ledger.clone()),
        &credential,
        tokio::time::Instant::now() + std::time::Duration::from_millis(200),
    )
    .await;

    let (reason, spend) = failed(&record);
    assert_eq!(reason, "deadline_exceeded");
    assert_eq!(ledger.settled(), vec![GRANT]);
    assert_eq!(spend.submitted_usd(), GRANT);
}

/// **Row 3c: a response over the bound books the estimate.** The service
/// answered at length, which is a billed answer this deployment refused to
/// buffer, not a free one.
#[tokio::test]
async fn a_response_over_the_bound_books_the_estimate() {
    let huge: &'static str =
        Box::leak("x".repeat(limits().max_response_bytes + 1).into_boxed_str());
    let (addr, _up) = upstream(huge).await;
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    let (reason, spend) = failed(&record);
    assert_eq!(reason, "response_too_large");
    assert_eq!(ledger.settled(), vec![GRANT]);
    assert_eq!(spend.submitted_usd(), GRANT);
}

/// **Row 3d: an envelope that does not parse books the estimate.** A 200 came
/// back, so the service did the work it was asked for.
#[tokio::test]
async fn a_malformed_envelope_books_the_estimate() {
    let (addr, _up) = upstream("<html>502</html>").await;
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    let (reason, spend) = failed(&record);
    assert_eq!(reason, "malformed");
    assert_eq!(ledger.settled(), vec![GRANT]);
    assert_eq!(spend.submitted_usd(), GRANT);
}

/// **Row 4: a reply with no usable usage books the estimate.** The answer
/// arrived and was billed at an amount the service did not report.
#[tokio::test]
async fn a_reply_without_usable_usage_books_the_estimate() {
    let (addr, _up) = upstream(ANSWER_PARTIAL_USAGE).await;
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    assert!(record.outcome.classification().is_some(), "{record:?}");
    assert_eq!(ledger.settled(), vec![GRANT]);
    assert_eq!(
        record.outcome.spend().copied(),
        Some(EvaluationSpend::Unknown {
            granted_usd: GRANT,
            settled: SettlementAck::Committed,
            submitted_usd: GRANT,
        })
    );
}

/// **Row 5, the control: a reply with usage settles what was measured.**
/// Without it every case above passes for a shadow that books the grant on
/// every call, which would over-count every measured one.
#[tokio::test]
async fn a_reply_with_usage_settles_the_measured_price_not_the_estimate() {
    let (addr, _up) = upstream(ANSWER).await;
    let ledger = RecordingLedger::granting(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow(addr, config(), ledger.clone()),
        &credential,
        never(),
    )
    .await;

    assert_eq!(ledger.settled(), vec![REPORTED_USD]);
    assert!(matches!(
        record.outcome.spend(),
        Some(EvaluationSpend::Measured { usd, .. }) if *usd == REPORTED_USD
    ));
}

/// **Every failure class, including the ones no socket test can reach.**
///
/// A client build failure and a header failure never reach a send (the first
/// fails construction, the second is refused in `prepare`), so only the pure
/// rule can show what they would book. The pre-socket refusals are listed for
/// the same reason: the match is exhaustive, and each arm's answer is a claim.
#[test]
fn the_failure_booking_rule_names_every_class() {
    let never_sent = [
        SystemOneError::Transport {
            message: "could not build an HTTP client".into(),
            timed_out: false,
            sent: false,
        },
        SystemOneError::Transport {
            message: "connection refused".into(),
            timed_out: true,
            sent: false,
        },
        SystemOneError::Status { status: 529 },
        SystemOneError::ForwardedCredentialRefused,
        SystemOneError::NoQuestions,
        SystemOneError::RequestTooLarge {
            limit_bytes: 1,
            actual_bytes: 2,
        },
    ];
    for error in &never_sent {
        assert_eq!(failure_submission_usd(error, GRANT), 0.0, "{error:?}");
    }
    let may_have_been_billed = [
        SystemOneError::Transport {
            message: "connection reset".into(),
            timed_out: false,
            sent: true,
        },
        SystemOneError::Transport {
            message: "operation timed out".into(),
            timed_out: true,
            sent: true,
        },
        SystemOneError::DeadlineExceeded,
        SystemOneError::ResponseTooLarge { limit_bytes: 1 },
        SystemOneError::Malformed,
    ];
    for error in &may_have_been_billed {
        assert_eq!(failure_submission_usd(error, GRANT), GRANT, "{error:?}");
    }
}

/// **An unconfirmed estimate is repaired at the same estimate, not at zero.**
///
/// The first settle is refused, so the record carries `Unconfirmed`. The
/// amount a repair re-drives is read off that record the way the session fold
/// reads it, then handed to a second shadow over a ledger that settles: the
/// repair must submit exactly what the first settle did. A repair at zero would
/// hand back the hold for a call the service may have billed.
#[tokio::test]
async fn an_unconfirmed_estimate_is_repaired_at_the_same_estimate() {
    let (addr, _up) = upstream("<html>502</html>").await;
    let refusing = RecordingLedger::granting_but_unsettleable(GRANT);
    let credential = credential();

    let record = classify_by(
        &shadow(addr, config(), refusing.clone()),
        &credential,
        never(),
    )
    .await;
    assert_eq!(
        refusing.settled(),
        vec![GRANT],
        "the first settle booked the estimate"
    );
    let spend = record.outcome.spend().expect("a call was made");
    assert_eq!(spend.settled(), SettlementAck::Unconfirmed);

    let unconfirmed = UnconfirmedSettlement {
        call_id: record.call_id.clone(),
        usd: spend
            .unconfirmed_settlement_usd()
            .expect("an unconfirmed settle is something to repair"),
    };
    let settling = RecordingLedger::granting(GRANT);
    let repaired = shadow(addr, config(), settling.clone())
        .repair_settlement(
            &Principal::new("proj_shadow", "user_shadow"),
            &SessionId::new("sess_shadow"),
            &unconfirmed,
            BudgetWindow::Total,
            never(),
        )
        .await;

    assert_eq!(repaired, Some(true));
    assert_eq!(
        settling.settled(),
        vec![GRANT],
        "the repair re-drives the amount the first settle submitted"
    );
    assert!(
        settling.requested().is_empty(),
        "and opens no second hold to do it"
    );
}
