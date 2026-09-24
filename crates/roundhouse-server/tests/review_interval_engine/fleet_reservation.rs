// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The real judge: exact prompt bytes and the reservation.
//!
//! Split from review_interval_engine.rs to keep each claim's file under
//! 1000 lines; the shared fixtures -- `Rig`,
//! `fleet_rig`'s callers, `shadow` and the log-reading helpers -- stay in the
//! parent module, reached here through `use super::*`. Every other claim in
//! that suite runs the scripted judge; these two use the real [`FleetJudge`]
//! over a capturing transport and a recording spend ledger, which is why they
//! alone need `CapturingJudgeTransport` and `ReservationProbe` below.

use super::*;

// ---------------------------------------------------------------------------
// The real judge: exact prompt bytes and the reservation
// ---------------------------------------------------------------------------

/// Records every judge quote and answers on track.
#[derive(Default)]
struct CapturingJudgeTransport {
    prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl FrontierClient for CapturingJudgeTransport {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.prompts.lock().unwrap().push(quote.prompt.clone());
        Ok(FrontierChunk::whole_response(
            ON_TRACK.to_string(),
            quote.prompt.len() as u64,
            0,
            CacheReadSource::Provider,
            40,
            0,
        ))
    }
}

/// Delegates to a memory ledger, records judge grant requests, and grants a
/// judge at most `cap` when one is set.
struct ReservationProbe {
    inner: MemorySpendLedger,
    requested: Mutex<Vec<f64>>,
    cap: Mutex<Option<f64>>,
}

impl ReservationProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySpendLedger::new(),
            requested: Mutex::new(Vec::new()),
            cap: Mutex::new(None),
        })
    }
}

#[async_trait]
impl SpendLedger for ReservationProbe {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        let judge = request.session_id.as_str().ends_with("#validate");
        if judge {
            self.requested.lock().unwrap().push(request.requested_usd);
        }
        let mut grant = self.inner.open_grant(request).await?;
        if judge && let Some(cap) = *self.cap.lock().unwrap() {
            grant.granted_usd = grant.granted_usd.min(cap);
        }
        Ok(grant)
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        self.inner.settle_grant(settlement).await
    }

    async fn balance(&self, query: BalanceQuery) -> Result<Balance, SpendError> {
        self.inner.balance(query).await
    }
}

fn funded() -> Admission {
    Admission {
        budget: Some(BudgetTerms {
            budget: Budget {
                limit_usd: 100.0,
                window: BudgetWindow::Total,
                on_exhaustion: Exhaustion::degrade_with_overflow(),
                warn_at: DEFAULT_WARN_AT,
            },
            allocation: Allocation::Pooled,
        }),
        ..shadow()
    }
}

fn estimate(prompt: &str) -> f64 {
    judge_spec().pricing.price(&Usage {
        input_tokens: ByteTokenizer.encode(prompt).len() as u64,
        cached_input_tokens: 0,
        cache_read_source: CacheReadSource::Unreported,
        cache_write_tokens: 0,
        output_tokens: JudgeConfig::default().expected_output_tokens as u64,
        reasoning_tokens: 0,
        accounting: Accounting::Estimated,
    })
}

fn fleet_rig(transport: &Arc<CapturingJudgeTransport>, probe: &Arc<ReservationProbe>) -> Rig {
    let judge = FleetJudge::new(
        Arc::clone(transport) as Arc<dyn FrontierClient>,
        judge_spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    )
    .with_spend_ledger(Arc::clone(probe) as Arc<dyn SpendLedger>);
    rig(
        Arc::new(judge),
        DEFAULT_INTERVAL_SECTION_BYTES,
        Arc::clone(probe) as Arc<dyn SpendLedger>,
    )
}

/// The recorded digest is the digest of the bytes the transport sent, and the
/// reservation is priced on those bytes, section included.
#[tokio::test]
async fn the_review_names_the_sent_bytes_and_reserves_for_them() {
    let transport = Arc::new(CapturingJudgeTransport::default());
    let probe = ReservationProbe::new();
    let rig = fleet_rig(&transport, &probe);
    let id = SessionId::new("acme/ada/bytes");
    rig.turn(&id, "t0", vec![Item::user_text("q0")], &funded())
        .await;
    rig.turn(&id, "t1", vec![Item::user_text("q1")], &funded())
        .await;

    let prompt = transport.prompts.lock().unwrap()[0].clone();
    let review = reviews(&rig.events(&id).await).pop().expect("a review").1;
    assert_eq!(
        review.prompt_digest,
        hex::encode(Sha256::digest(prompt.as_bytes()))
    );
    assert!(prompt.contains(INTERVAL_SECTION_HEADING), "{prompt}");
    let classic = &prompt[..prompt
        .find(&format!("\n{INTERVAL_SECTION_HEADING}"))
        .unwrap()];
    let requested = probe.requested.lock().unwrap()[0];
    assert!(
        (requested - estimate(&prompt)).abs() < 1e-12,
        "{requested} against {}",
        estimate(&prompt)
    );
    assert!(requested > estimate(classic), "the section is reserved for");
    assert_eq!(review.label, IntervalLabel::Positive);
}

/// A reservation that covers the classic brief but not the section refuses the
/// review, and the interval stays open for the next funded one.
#[tokio::test]
async fn a_refused_reservation_leaves_the_interval_open() {
    // Measure the two prompt sizes on an identical session.
    let transport = Arc::new(CapturingJudgeTransport::default());
    let probe = ReservationProbe::new();
    let measure = fleet_rig(&transport, &probe);
    let id = SessionId::new("acme/ada/measure");
    measure
        .turn(&id, "t0", vec![Item::user_text("q0")], &funded())
        .await;
    measure
        .turn(&id, "t1", vec![Item::user_text("q1")], &funded())
        .await;
    let prompt = transport.prompts.lock().unwrap()[0].clone();
    let classic = prompt[..prompt
        .find(&format!("\n{INTERVAL_SECTION_HEADING}"))
        .unwrap()]
        .to_string();
    let (small, large) = (estimate(&classic), estimate(&prompt));

    let transport = Arc::new(CapturingJudgeTransport::default());
    let probe = ReservationProbe::new();
    *probe.cap.lock().unwrap() = Some((small + large) / 2.0);
    let rig = fleet_rig(&transport, &probe);
    let id = SessionId::new("acme/ada/refused");
    rig.turn(&id, "t0", vec![Item::user_text("q0")], &funded())
        .await;
    rig.turn(&id, "t1", vec![Item::user_text("q1")], &funded())
        .await;
    assert!(
        transport.prompts.lock().unwrap().is_empty(),
        "refused before the socket"
    );
    assert!(reviews(&rig.events(&id).await).is_empty());
    let replayed = rig.replay(&id).await;
    assert_eq!(
        replayed.review_checkpoint(),
        0,
        "a refusal is not a checkpoint"
    );
    assert_eq!(replayed.pending_review_decisions().count(), 2);

    *probe.cap.lock().unwrap() = None;
    rig.turn(&id, "t2", vec![Item::user_text("q2")], &funded())
        .await;
    let events = rig.events(&id).await;
    let review = reviews(&events).pop().expect("the funded review").1;
    assert_eq!(review.after_seq, 0);
    assert_eq!(
        covered(&review),
        routed(&events)[..2]
            .iter()
            .map(|(seq, _)| *seq)
            .collect::<Vec<_>>()
    );
}
