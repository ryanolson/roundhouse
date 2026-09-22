// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Settlement ordering against the real `MemorySpendLedger`. `RecordingLedger`,
//! used elsewhere in this module, records what it was told and deduplicates
//! nothing, so it cannot show this.
//!
//! Contract under test: two classification intents dispatched under one
//! session, each holding its own ledger key, must both commit their reported
//! usage and release their hold no matter which one completes first.
//!
//! **What made this fail.** Settled through a per-session watermark, the intent
//! that completed *second* but was issued *first* carried the lower log
//! position, so its settle was indistinguishable from a replay: its charge was
//! dropped and its hold left to lapse. `SettlementKey::OncePerCall` is what this
//! module now settles under, and it names the call's own hold — so there is no
//! ordering left for a completion order to violate. The pair is kept as a
//! regression: the guarantee is about the adapter's choice of key, and a
//! future edit that reached back for a session position would redden the
//! reversed case exactly as it did before.
//!
//! Sequential `.await`s in reversed order characterize completion *ordering*
//! deterministically; this does not exercise concurrent dispatch.

use roundhouse_core::control::{BalanceQuery, MemorySpendLedger};

use super::*;

const ANSWER_INTENT_A: &str = r#"{"model":"jev-1.12","answers":{"tier":{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}},"usage":{"input_tokens":200,"output_tokens":40}}"#;
const ANSWER_INTENT_B: &str = r#"{"model":"jev-1.12","answers":{"tier":{"type":"choice","choice":"capable","probabilities":{"capable":0.85,"efficient":0.15},"confidence":0.82}},"usage":{"input_tokens":300,"output_tokens":60}}"#;

/// One classification intent: its hold identity and the upstream usage it
/// reports. Amounts differ per intent so a sum assertion cannot pass on two
/// equal halves.
#[derive(Clone, Copy)]
struct Intent {
    hold_key: &'static str,
    input_tokens: u64,
    output_tokens: u64,
    answer_body: &'static str,
}

/// The intent issued first, and — in the reversed case — completing second.
const INTENT_A: Intent = Intent {
    hold_key: "hold_intent_a",
    input_tokens: 200,
    output_tokens: 40,
    answer_body: ANSWER_INTENT_A,
};
const INTENT_B: Intent = Intent {
    hold_key: "hold_intent_b",
    input_tokens: 300,
    output_tokens: 60,
    answer_body: ANSWER_INTENT_B,
};

/// Priced through the same call `TypeSafeShadow::price` makes, bit-for-bit.
fn expected_usd(input_tokens: u64, output_tokens: u64) -> f64 {
    pricing().price(&roundhouse_core::event::Usage {
        input_tokens,
        cached_input_tokens: 0,
        output_tokens,
        ..Default::default()
    })
}

fn shadow_on(addr: SocketAddr, ledger: Arc<MemorySpendLedger>) -> TypeSafeShadow<ByteTokenizer> {
    let client = SystemOneClient::new(format!("http://{addr}"), limits()).unwrap();
    TypeSafeShadow::new(client, config().enable(), ledger, ByteTokenizer)
}

/// The `(usage, usd)` a call was priced at, or a panic naming what came back.
fn measured(outcome: &ShadowOutcome) -> (SystemOneUsage, f64) {
    match outcome {
        ShadowOutcome::Answered {
            accounting: Accounting::Measured { usage, usd },
            ..
        } => (*usage, *usd),
        other => panic!("expected a priced answer, got {other:?}"),
    }
}

/// Run both intents under one session and one fresh evaluation ledger, in
/// `completion_order`, then assert the ledger committed both intents' usage
/// and released both holds. The two named tests below supply the two orders;
/// the assertions are order-invariant by construction.
async fn assert_both_intents_settle(completion_order: [Intent; 2]) {
    let ledger = Arc::new(MemorySpendLedger::new());
    let credential = credential();
    let principal = Principal::new("proj_settlement_order", "user_settlement_order");
    let session_id = SessionId::new("sess_settlement_order");
    let pool = Pool::of(vec![frontier()]);

    for (i, intent) in completion_order.into_iter().enumerate() {
        let (addr, up) = upstream(intent.answer_body).await;
        let call = ShadowCall {
            principal: principal.clone(),
            session_id: session_id.clone(),
            hold_key: ResponseId::new(intent.hold_key),
            terms: terms(),
            credential: &credential,
            now_ms: 1_000 + i as u64 * 500,
        };
        let outcome = shadow_on(addr, ledger.clone())
            .classify(
                call,
                &items(),
                Objective::Unknown,
                Vec::new(),
                &pool.admitted(),
            )
            .await;
        assert_eq!(up.count(), 1, "{} must reach its upstream", intent.hold_key);
        assert_eq!(
            measured(&outcome),
            (
                SystemOneUsage {
                    input_tokens: intent.input_tokens,
                    output_tokens: intent.output_tokens,
                },
                expected_usd(intent.input_tokens, intent.output_tokens),
            ),
            "{} must be priced from what it reported",
            intent.hold_key
        );
    }

    let balance = ledger
        .balance(BalanceQuery {
            principal,
            terms: terms(),
            now_ms: 1_500,
        })
        .await
        .expect("a balance read against the fixture terms");
    let expected_committed = expected_usd(INTENT_A.input_tokens, INTENT_A.output_tokens)
        + expected_usd(INTENT_B.input_tokens, INTENT_B.output_tokens);
    assert_eq!(
        balance.committed_usd, expected_committed,
        "settlement must commit the sum of both intents' usage regardless of completion order"
    );
    assert_eq!(
        balance.held_usd, 0.0,
        "settlement must release both holds regardless of completion order"
    );
}

/// Control: the intent issued first also completes first.
#[tokio::test]
async fn in_order_completion_commits_the_sum_of_both_intents() {
    assert_both_intents_settle([INTENT_A, INTENT_B]).await;
}

/// The intent issued second completes first, same session — the case a
/// session-scoped watermark mistook for a stale replay of the other one.
#[tokio::test]
async fn reversed_completion_must_still_commit_the_sum_of_both_intents() {
    assert_both_intents_settle([INTENT_B, INTENT_A]).await;
}
