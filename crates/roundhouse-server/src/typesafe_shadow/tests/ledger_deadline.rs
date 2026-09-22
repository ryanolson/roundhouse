// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The call's one absolute deadline, aimed at the ledger directly.
//!
//! `classify_runtime`'s own tests reach these states through the real worker,
//! which is what proves the runtime hands `execute` the instant it computed.
//! Here the deadline is a parameter, so each case is a short wait against a
//! ledger that is *deterministically* pending rather than an 800ms poll against
//! one that merely happens to be slow — and every ledger wait `execute` can
//! reach gets its own case, including the one inside `reserve` that no
//! successful call ever runs.
//!
//! **One deadline, four waits.** Queue, grant, send and settle spend the same
//! instant. A settle abandoned at it is *unacknowledged*, not rolled back: the
//! hold this process stopped waiting on lapses on its own TTL, which is the
//! backend's cleanup window and not a second deadline for this worker.

use super::*;

/// A ledger whose `open_grant` and `settle_grant` can each be made to never
/// answer, so a deadline is the only thing that can end the wait.
///
/// Pending forever rather than slow: a sleep long enough to look stalled would
/// make every assertion here a race against a machine's load, and one short
/// enough not to would prove nothing.
struct StallingLedger {
    /// What a grant answers, or `None` to never answer at all.
    grants: Option<f64>,
    /// `false` makes `settle_grant` hang rather than fail — the state a
    /// timeout is about, and a different one from the rejection
    /// `RecordingLedger::granting_but_unsettleable` produces.
    settles: bool,
    settles_attempted: Arc<AtomicUsize>,
}

impl StallingLedger {
    /// A grant that never answers. Nothing past it can run.
    fn grant_never_answers() -> Arc<Self> {
        Arc::new(Self {
            grants: None,
            settles: true,
            settles_attempted: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// A grant that answers `granted_usd` at once, and a settle that never
    /// answers.
    fn settle_never_answers(granted_usd: f64) -> Arc<Self> {
        Arc::new(Self {
            grants: Some(granted_usd),
            settles: false,
            settles_attempted: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn settles_attempted(&self) -> usize {
        self.settles_attempted.load(Ordering::SeqCst)
    }

    /// A grant and a settle that both answer at once, with no await point in
    /// either arm. Unlike [`Self::settle_never_answers`], whose settle hangs,
    /// this is the shape an already-past-deadline hypothesis about
    /// `timeout_at`'s poll order needs undiluted: nothing here ever leaves
    /// this test waiting on anything but the call's own deadline.
    fn always_ready(granted_usd: f64) -> Arc<Self> {
        Arc::new(Self {
            grants: Some(granted_usd),
            settles: true,
            settles_attempted: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl SpendLedger for StallingLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        match self.grants {
            Some(granted_usd) => Ok(Grant {
                granted_usd,
                state: LedgerState::Unconstrained,
            }),
            None => {
                let _ = request;
                std::future::pending().await
            }
        }
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        // Counted before the stall, so a test can say the settle was *reached*
        // and then abandoned rather than never attempted.
        self.settles_attempted.fetch_add(1, Ordering::SeqCst);
        if !self.settles {
            std::future::pending::<()>().await;
        }
        Ok(Settled {
            applied: true,
            released_usd: 0.0,
            committed_usd: settlement.actual_usd,
        })
    }

    async fn balance(
        &self,
        _query: roundhouse_core::control::BalanceQuery,
    ) -> Result<roundhouse_core::control::Balance, SpendError> {
        unimplemented!("no test here reads a balance")
    }
}

fn stalling_shadow(addr: SocketAddr, ledger: Arc<StallingLedger>) -> TypeSafeShadow<ByteTokenizer> {
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();
    TypeSafeShadow::new(client, config().enable(), ledger, ByteTokenizer)
}

/// A deadline close enough to be reached inside a test, and far enough that the
/// work before it is not racing it.
fn in_150ms() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_millis(150)
}

/// Prepare one call the way the engine does, without executing it.
fn prepared(
    shadow: &TypeSafeShadow<ByteTokenizer>,
    credential: &TurnCredential,
) -> crate::typesafe_shadow::PreparedCall {
    let projection = shadow.projection(&capture(), &[], &[]).expect("it fits");
    shadow
        .prepare(call(credential), &projection, Some(&[frontier()]))
        .expect("prepared")
}

/// **A grant that never answers is abandoned at the call's deadline, and no
/// request is sent afterwards.**
///
/// `Unfunded` with no [`EvaluationSpend`] at all is the honest record: no socket
/// was touched, so there is no provider charge to be uncertain about. It says
/// nothing either way about whether the *ledger* opened a hold before this
/// worker stopped waiting for its answer — a grant abandoned mid-flight can
/// leave one, and it lapses on its TTL.
#[tokio::test]
async fn a_grant_that_never_answers_is_abandoned_at_the_calls_deadline() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = StallingLedger::grant_never_answers();
    let shadow = stalling_shadow(addr, ledger.clone());
    let credential = credential();

    let record = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(prepared(&shadow, &credential), in_150ms()),
    )
    .await
    .expect("the grant wait must be bounded by the call's own deadline");

    assert_eq!(
        record.outcome,
        ClassificationOutcome::Unfunded {
            reason: FundingRefusal::Expired
        },
        "a call whose grant never answered inside its own life sent nothing"
    );
    assert_eq!(
        up.count(),
        0,
        "and no request may be bought for a call whose life ran out while its \
         grant was still open"
    );
    assert_eq!(
        ledger.settles_attempted(),
        0,
        "nothing was granted, so there is no hold this worker could settle"
    );
}

/// **A settle that never answers is abandoned at the same deadline, and the
/// completed round trip keeps everything it established.**
///
/// Exact usage, the exact amount this deployment's rate card prices it at, the
/// answer itself and the identity that produced it are all facts about a round
/// trip that finished. Only the acknowledgement is unknown, and
/// [`SettlementAck::Unconfirmed`] is where that goes.
#[tokio::test]
async fn a_settle_that_never_answers_leaves_the_measured_spend_unconfirmed() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = StallingLedger::settle_never_answers(1_000.0);
    let shadow = stalling_shadow(addr, ledger.clone());
    let credential = credential();

    let record = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(prepared(&shadow, &credential), in_150ms()),
    )
    .await
    .expect("the settle must be bounded by the call's own deadline");

    assert_eq!(up.count(), 1, "the call itself succeeded");
    assert_eq!(
        ledger.settles_attempted(),
        1,
        "and its settle was reached and then abandoned, not skipped"
    );
    assert_eq!(
        record.outcome,
        ClassificationOutcome::Classified {
            classification: TurnClassification {
                taxonomy_version: TAXONOMY_VERSION,
                intent: Graded {
                    value: TurnIntent::Implement,
                    confidence: 0.82
                },
                complexity: Graded {
                    value: TurnComplexity::Involved,
                    confidence: 0.61
                },
                context_dependence: Graded {
                    value: ContextDependence::Recent,
                    confidence: 0.55
                },
            },
            spend: EvaluationSpend::Measured {
                usage: EvaluationUsage {
                    input_tokens: 312,
                    output_tokens: 48,
                },
                usd: REPORTED_USD,
                granted_usd: 1_000.0,
                settled: SettlementAck::Unconfirmed,
            },
            reported_model: Some("jev-1.12".to_string()),
        },
        "an abandoned settle discards nothing the call established"
    );
    assert_eq!(
        record.outcome.committed_usd(),
        None,
        "and nothing this deployment can point at says the charge landed"
    );
}

/// **The cleanup settle inside `reserve` is under the same deadline**, which
/// matters because it is the one ledger wait on a path that will never send a
/// request at all: a partial grant is handed straight back before the refusal
/// is returned.
#[tokio::test]
async fn a_partial_grants_cleanup_settle_is_abandoned_at_the_deadline_too() {
    let (addr, up) = upstream(ANSWER).await;
    // Zero against a nonzero quote: a partial reservation, whose release is the
    // settle that then never answers.
    let ledger = StallingLedger::settle_never_answers(0.0);
    let shadow = stalling_shadow(addr, ledger.clone());
    let credential = credential();
    let call = prepared(&shadow, &credential);
    let requested_usd = call.intent.reservation.requested_usd;
    assert!(
        requested_usd > 0.0,
        "the quote must be nonzero for a zero grant to be a partial one"
    );

    let record = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(call, in_150ms()),
    )
    .await
    .expect("the cleanup settle must be bounded by the call's own deadline");

    assert_eq!(
        record.outcome,
        ClassificationOutcome::Unfunded {
            reason: FundingRefusal::BudgetRefused {
                requested_usd,
                granted_usd: 0.0,
            }
        },
        "a cleanup settle running out of time does not turn a budget refusal \
         into an expiry: both amounts the refusal established survive"
    );
    assert_eq!(
        ledger.settles_attempted(),
        1,
        "the short hold's release was reached and then abandoned"
    );
    assert_eq!(up.count(), 0, "and a refused budget sends nothing");
}

/// **A grant that requires no wait at all is exactly the future `timeout_at`
/// returns without ever consulting the deadline.**
///
/// `timeout_at` polls its own future before it looks at the clock —
/// `classify_runtime::run` documents this same fact and re-checks the clock
/// itself after its own `timeout_at` returns, specifically because "an
/// uncontended permit ... resolves immediately and a call whose life had
/// already run out would be sent anyway." `execute` has no caller standing
/// between it and an already-expired deadline in this test, so this is the
/// sharpest case available for asking whether `execute` carries the same
/// protection on its own: a grant with no await point in it at all, asked for
/// under a deadline that had already passed before `execute` was ever called.
///
/// The grant answering despite the expired deadline is the hypothesized
/// symptom, not the thing under test — what decides whether a request reaches
/// the wire is `send_and_settle`'s own `timeout_at`, over a future that
/// cannot complete without a socket. The counted loopback below is what
/// settles it.
#[tokio::test]
async fn a_deadline_already_past_when_execute_is_called_sends_no_request_despite_an_instant_grant()
{
    let (addr, up) = upstream(ANSWER).await;
    let ledger = StallingLedger::always_ready(1_000.0);
    let shadow = stalling_shadow(addr, ledger.clone());
    let credential = credential();
    let call = prepared(&shadow, &credential);

    // Already past by construction, not by waiting: nothing runs between this
    // line and `execute` that could make the expiry a race with the
    // scheduler rather than a fact of how the deadline was built.
    let already_expired = tokio::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(50))
        .expect("the runtime clock has been up longer than 50ms");

    let record = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(call, already_expired),
    )
    .await
    .expect("an already-expired deadline must not hang");

    assert_eq!(
        up.count(),
        0,
        "a call whose deadline had already passed before execute was even \
         called must buy no classifier request, whatever its grant answered"
    );
    assert_eq!(
        record.outcome.committed_usd(),
        None,
        "and nothing on the record may say a charge landed for a request \
         that was never sent"
    );
}

/// **The positive control.** The same instantly-answering ledger, against a
/// deadline nowhere near expiry, does reach the upstream — proof that the
/// zero count above is evidence of a boundary the expired deadline enforces
/// and not of a counter, a client, or a ledger fixture that never moves.
#[tokio::test]
async fn the_same_instant_grant_does_reach_the_upstream_when_its_deadline_has_not_passed() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = StallingLedger::always_ready(1_000.0);
    let shadow = stalling_shadow(addr, ledger.clone());
    let credential = credential();
    let call = prepared(&shadow, &credential);

    let record = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(call, in_150ms()),
    )
    .await
    .expect("an unexpired deadline must not hang");

    assert_eq!(
        up.count(),
        1,
        "an ordinary call under an unexpired deadline does reach the upstream"
    );
    assert!(
        matches!(record.outcome, ClassificationOutcome::Classified { .. }),
        "and it classifies cleanly, which is what makes the zero count above \
         evidence of a boundary rather than of a broken fixture"
    );
}

/// A short, bounded wait for the upstream's own accept-and-parse task to
/// catch up, rather than reading [`Upstream::count`] on this task's very
/// next instruction.
///
/// A request that reached a real socket needs the server side's own task
/// turn to accept, read and parse it — asynchronous by nature, and on a
/// single-threaded test runtime that turn is not guaranteed to have run by
/// the time `execute` returns to its caller. Polling once and stopping as
/// soon as two consecutive reads agree is what makes a leaked request
/// visible without waiting the full bound on the ordinary, no-leak path.
async fn drained_count(up: &Upstream, bound: std::time::Duration) -> usize {
    let deadline = tokio::time::Instant::now() + bound;
    let mut last = up.count();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let now = up.count();
        if now == last || tokio::time::Instant::now() >= deadline {
            return now;
        }
        last = now;
    }
}

/// **The same shadow, the same pooled client, warmed by a real call before
/// the expired one — and a further valid call to prove it still works.**
///
/// Every case above builds a fresh [`TypeSafeShadow`] — and so a fresh
/// [`SystemOneClient`], and so a fresh, cold TCP connection — for each call.
/// That is exactly the condition under which `send`'s very first poll cannot
/// complete without an async wait: establishing a connection is itself
/// asynchronous, so `timeout_at` always finds it `Pending` and gets to check
/// an already-elapsed deadline before anything reaches a socket. Production
/// reuses one `TypeSafeShadow`, and so one pooled client, across every call a
/// session makes. A *warm*, already-connected socket removes that
/// asynchronous step from the write side: writing a small request onto a
/// socket the kernel already reports writable can succeed inside a single
/// poll, with nothing asynchronous about it until the response is read back.
/// Whether that changes anything a client-side outcome reports is already
/// answered by the case above — `send`'s `timeout_at` still finds *some*
/// `Pending` before the full response can be read, deadline or not — but
/// whether it changes what the *upstream* observes is a different, genuinely
/// unproven question, since a synchronous write does not need the deadline
/// check to run before it happens. This is the version of the test built to
/// ask that question directly, on the one channel a client-side assertion
/// cannot see: the server's own request count.
#[tokio::test]
async fn an_expired_call_on_a_warmed_shared_client_leaks_no_extra_request_between_two_valid_ones() {
    let (addr, up) = upstream(ANSWER).await;
    let ledger = StallingLedger::always_ready(1_000.0);
    // One shadow, and so one `SystemOneClient` and one connection pool,
    // across all three calls below — the opposite of the cases above, where
    // a fresh shadow gave every call its own cold connection.
    let shadow = stalling_shadow(addr, ledger.clone());
    let credential = credential();

    // Warm-up: a real, valid call over what becomes the pooled connection the
    // expired call is asked to reuse.
    let warm_up = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(prepared(&shadow, &credential), in_150ms()),
    )
    .await
    .expect("the warm-up call must not hang");
    assert!(
        matches!(warm_up.outcome, ClassificationOutcome::Classified { .. }),
        "the warm-up call must itself succeed, or nothing after it is warmed \
         by anything"
    );

    // Same shadow, same client, same pool — already past by construction,
    // exactly as in the cold-connection case above, and now asked
    // immediately after a call that has already exercised this client's
    // connection once.
    let already_expired = tokio::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(50))
        .expect("the runtime clock has been up longer than 50ms");
    let expired = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(prepared(&shadow, &credential), already_expired),
    )
    .await
    .expect("an already-expired deadline must not hang even on a warm client");
    assert_eq!(
        expired.outcome.committed_usd(),
        None,
        "nothing on the record may say a charge landed for the expired call, \
         warm client or not"
    );

    // A further valid call: proof the shared client is still usable after
    // the expired one, and a second real round trip that gives any request
    // the expired call may have leaked strictly more time to reach the
    // upstream than an immediate read after this call alone would.
    let follow_up = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        shadow.execute(prepared(&shadow, &credential), in_150ms()),
    )
    .await
    .expect("the follow-up call must not hang");
    assert!(
        matches!(follow_up.outcome, ClassificationOutcome::Classified { .. }),
        "the shared client must still serve a valid call after the expired \
         one"
    );

    let settled = drained_count(&up, std::time::Duration::from_millis(300)).await;
    assert_eq!(
        settled, 2,
        "exactly the warm-up and the follow-up may reach the upstream; the \
         expired call between them must not leak a third"
    );
}
