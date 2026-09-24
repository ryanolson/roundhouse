// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The evaluation ledger is a separate ledger from the serving one, and
//! neither its grant nor its settle blocks a turn: both can be held open
//! indefinitely while the session's own turns keep completing.

use super::*;

/// The evaluation ledger is not the serving one, asserted through the boundary
/// that chooses both.
#[tokio::test]
async fn the_evaluation_ledger_is_a_different_ledger_from_the_serving_one() {
    use roundhouse_core::control::{
        Allocation, BalanceQuery, Budget, BudgetTerms, BudgetWindow, Exhaustion, GrantRequest,
        Principal,
    };
    use roundhouse_core::ids::ResponseId;
    use roundhouse_server::shared_backend;

    let namespace = shared_backend::resolve_namespace(None).expect("the default");
    let backends = shared_backend::open(None, &namespace)
        .await
        .expect("the per-process arm needs no Redis");

    let terms = BudgetTerms {
        budget: Budget {
            limit_usd: 100.0,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::Refuse,
            warn_at: 0.8,
        },
        allocation: Allocation::Pooled,
    };
    let principal = Principal::new("proj_split", "user_split");
    let serving = match &backends {
        roundhouse_server::Backends::PerProcess { spend, .. } => Arc::clone(spend),
        _ => panic!("the per-process arm was expected"),
    };
    let evaluation = Arc::clone(backends.evaluation_spend());

    evaluation
        .open_grant(GrantRequest {
            principal: principal.clone(),
            session_id: SessionId::new("sess_split"),
            response_id: ResponseId::new("eval_1"),
            requested_usd: 5.0,
            ttl_ms: 60_000,
            terms: terms.clone(),
            now_ms: 1_000,
        })
        .await
        .expect("the evaluation ledger grants");

    let serving_balance = serving
        .balance(BalanceQuery {
            principal: principal.clone(),
            terms: terms.clone(),
            now_ms: 1_000,
        })
        .await
        .expect("a balance");
    assert_eq!(
        serving_balance.held_usd, 0.0,
        "a classification must not be able to spend a project's serving budget"
    );
    let evaluation_balance = evaluation
        .balance(BalanceQuery {
            principal,
            terms,
            now_ms: 1_000,
        })
        .await
        .expect("a balance");
    assert_eq!(evaluation_balance.held_usd, 5.0);
}

/// **The claim.** `TypeSafeShadow::execute` is what awaits the evaluation
/// ledger, and it runs on the background worker `classifier.spawn` starts --
/// never on the turn that calls `request_classification`. Holding both
/// `open_grant` and, separately, `settle_grant` open for the whole test proves
/// neither is a routing dependency two ways: the dispatching turn itself
/// completes while its own call is parked in each, and — the claim a single
/// turn's own latency cannot make — a **second** and **third** turn run to
/// completion afterward, each while a different gate is still held, so
/// nothing about the session is blocked on the ledger either.
///
/// `max_in_flight: 1` keeps the proof attributable to turn one's call alone:
/// its worker holds the runtime's only admission permit for the whole stall,
/// so turns two and three find no capacity and skip classification (the
/// ordinary, already-tested fail-open path) rather than opening calls of
/// their own that would blur which call is being held.
///
/// Every wait below synchronizes on the ledger actually entering the call
/// (`StallingLedger`'s `*_entered` notifications) rather than sleeping a
/// guessed duration, with a bounded `tokio::time::timeout` as the outer
/// safety net against a real hang.
#[tokio::test]
async fn a_stalled_evaluation_grant_and_settlement_do_not_block_the_turn() {
    let (base_url, upstream) = classifier_upstream().await;
    let mut config = config(&base_url, true);
    config.executor.max_in_flight = 1;
    let ledger = StallingLedger::new();
    let runtime = compose(
        "<test>",
        &config,
        ledger.clone() as Arc<dyn roundhouse_core::control::SpendLedger>,
        ByteTokenizer,
        &env,
    )
    .expect("it composes")
    .expect("and is present");

    let registry = FrontierClients::keyed(
        [(
            PROVIDER.to_string(),
            Arc::new(Answering) as Arc<dyn FrontierClient>,
        )]
        .into_iter()
        .collect(),
    );
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        Engine::with_provider_clients(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local")) as Arc<dyn LocalExecutor>,
            catalog(),
            Arc::new(registry),
            Arc::new(AffinityPolicy::new()),
            EngineConfig {
                turn_deadline_ms: 5_000,
                ..EngineConfig::default()
            },
        )
        .with_classifier(Arc::clone(&runtime)),
    );
    let session = SessionId::new("sess_stalled_ledger");
    engine.create_session(&session).await.unwrap();

    let sync_bound = Duration::from_secs(5);
    let turn = |name: &'static str, text: &'static str| {
        let engine = Arc::clone(&engine);
        let session = session.clone();
        async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                engine.run_turn(
                    &session,
                    TurnId::new(name),
                    vec![Item::user_text(text)],
                    &Admission::open(),
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("turn {name} must complete well within its deadline"))
            .expect("this fleet always answers")
        }
    };

    turn("t1", "fix the parser and prove it").await;

    // Synchronize on the worker genuinely being parked inside `open_grant`,
    // not on a guessed sleep.
    tokio::time::timeout(sync_bound, ledger.open_grant_entered.notified())
        .await
        .expect("open_grant must actually be entered for this test to be about anything");

    // The intent is durable -- the turn's own writer wrote it -- and the
    // ledger has been *asked*, but the worker is parked inside `open_grant`
    // and has sent nothing.
    let events = store.read_events(&session, 0, 1_000).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ClassificationRequested { .. })),
        "the durable intent must exist before any grant is asked for"
    );
    assert_eq!(
        upstream.count(),
        0,
        "nothing was sent while the grant is held"
    );

    // The claim: a second turn, in the same session, runs to completion while
    // the first turn's classification is still stuck inside `open_grant`.
    turn("t2", "now add a test").await;

    // Release the grant. The worker proceeds to the HTTP call and then to
    // `settle_grant`, which this ledger is *also* holding -- a separate gate,
    // so this is a distinct proof from the one above rather than the same
    // stall observed twice.
    ledger.release_open_grant();
    tokio::time::timeout(sync_bound, ledger.settle_entered.notified())
        .await
        .expect("settle_grant must actually be entered once the grant releases");
    assert_eq!(
        upstream.count(),
        1,
        "the call must have reached the classifier by the time settlement is \
         entered, since settlement only follows a reply"
    );

    // The same claim again, for settlement: a third turn runs to completion
    // while the first turn's call is stuck inside `settle_grant`.
    turn("t3", "and one more").await;

    ledger.release_settle();
    let mut delivered = Vec::new();
    for _ in 0..200 {
        delivered = runtime.ready(&session).await;
        if !delivered.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(delivered.len(), 1, "the call settles and parks its result");
}
