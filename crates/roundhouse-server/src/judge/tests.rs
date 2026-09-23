// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `judge` under test.

use super::*;
use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Allocation, Balance, BalanceQuery, Budget, BudgetWindow, DEFAULT_WARN_AT, Exhaustion,
    MemorySpendLedger, Principal,
};
use roundhouse_core::ids::{SessionId, SideCallId};
use roundhouse_core::routing::{CacheModel, ProviderPricing};
use roundhouse_fleet::WireProtocol;
use std::sync::Mutex;

/// A client that records the quote it was handed and answers from a script.
#[derive(Default)]
struct RecordingClient {
    seen: Mutex<Vec<FrontierQuote>>,
    fail: Option<FrontierError>,
}

#[async_trait]
impl FrontierClient for RecordingClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.seen.lock().expect("recording").push(quote.clone());
        match &self.fail {
            Some(FrontierError::Upstream(message)) => Err(FrontierError::Upstream(message.clone())),
            Some(FrontierError::UnknownProvider(name)) => {
                Err(FrontierError::UnknownProvider(name.clone()))
            }
            Some(FrontierError::Credential(error)) => Err(FrontierError::Credential(error.clone())),
            Some(FrontierError::MalformedQuote(why)) => {
                Err(FrontierError::MalformedQuote(why.clone()))
            }
            Some(FrontierError::UnsupportedDialect {
                expected,
                got,
                target,
            }) => Err(FrontierError::UnsupportedDialect {
                expected,
                got,
                target: target.clone(),
            }),
            Some(FrontierError::UntranslatableTools { tool, from, to }) => {
                Err(FrontierError::UntranslatableTools {
                    tool: tool.clone(),
                    from,
                    to,
                })
            }
            Some(FrontierError::Transport { message, timed_out }) => {
                Err(FrontierError::Transport {
                    message: message.clone(),
                    timed_out: *timed_out,
                })
            }
            Some(FrontierError::Status { status, message }) => Err(FrontierError::Status {
                status: *status,
                message: message.clone(),
            }),
            None => Ok(FrontierChunk::whole_response(
                r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#
                    .to_string(),
                900,
                0,
                CacheReadSource::Provider,
                40,
                0,
            )),
        }
    }
}

/// A client that records the quote *and* answers without any accounting.
///
/// One fixture rather than two because the assertions this file needs most
/// are joins: what we counted the prompt as against what we sent, which no
/// fixture that supplies only one half can witness.
#[derive(Default)]
struct RecordingSilentClient {
    seen: Mutex<Vec<FrontierQuote>>,
}

#[async_trait]
impl FrontierClient for RecordingSilentClient {
    async fn execute(&self, quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        self.seen.lock().expect("recording").push(quote.clone());
        Ok(futures::stream::iter([Ok(FrontierChunk::OutputText(
            r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#
                .to_string(),
        ))])
        .boxed())
    }
}

fn recording_judge(
    spec: FrontierModelSpec,
) -> (Arc<RecordingSilentClient>, FleetJudge<ByteTokenizer>) {
    let client = Arc::new(RecordingSilentClient::default());
    let judge = FleetJudge::new(
        Arc::clone(&client) as Arc<dyn FrontierClient>,
        spec,
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    );
    (client, judge)
}

/// The one quote a check put on the wire.
async fn quote_of(
    client: &Arc<RecordingSilentClient>,
    judge: &FleetJudge<ByteTokenizer>,
    system_prompt: &str,
    brief: &str,
) -> FrontierQuote {
    judge
        .consult(&Check::nth(0).under(None), system_prompt, brief)
        .await
        .expect("a stream that ended is an answer");
    client.seen.lock().expect("recording").remove(0)
}

/// The reservation and transport use the same prompt, including the separator.
#[tokio::test]
async fn a_check_is_counted_on_the_bytes_it_sends() {
    let (client, judge) = recording_judge(spec());
    let answer = judge
        .consult(&Check::nth(0).under(None), "system", "brief")
        .await
        .expect("a stream that ended is an answer");
    let sent = client.seen.lock().expect("recording")[0].prompt.clone();

    assert_eq!(
        answer.usage.input_tokens,
        ByteTokenizer.encode(&sent).len() as u64,
        "the count and the transport must come off one prepared string, or \
         a check reserves for less than {sent:?}"
    );
}

/// The blocks a Messages client cuts are a slicing of the prompt that was
/// counted, and they name the one stretch of a check that repeats.
#[tokio::test]
async fn a_check_names_its_stable_system_prefix_as_one_segment() {
    let (client, judge) = recording_judge(spec());
    let quote = quote_of(&client, &judge, "system", "brief").await;

    let segments = quote
        .segments()
        .expect("a check's boundaries describe its own prompt");
    assert_eq!(
        segments.len(),
        2,
        "the system prompt is constant across every check and the brief is \
         not, so they are the two blocks: {segments:?}"
    );
    assert_eq!(
        segments.concat(),
        quote.prompt,
        "segments are a slicing of what was sent, never a second rendering"
    );
    assert_eq!(
        (segments[0], segments[1]),
        ("system\n\n", "brief"),
        "the boundary must preserve the fixed separator and full brief: {segments:?}"
    );
}

/// A check asks the provider for the lifetime its own target declares,
/// read by the rule the engine reads it by — one catalog field, so the TTL
/// the wire asks for and the TTL a rate card is held to cannot disagree.
#[tokio::test]
async fn a_check_asks_for_its_targets_declared_cache_lifetime() {
    for (cache_model, requested) in [
        (CacheModel::Deterministic { ttl_ms: 300_000 }, Some(300_000)),
        (
            CacheModel::Deterministic { ttl_ms: 3_600_000 },
            Some(3_600_000),
        ),
        (CacheModel::Observed, None),
        (
            CacheModel::InactivityDecay {
                half_life_ms: 60_000,
                max_ttl_ms: 600_000,
                min_prefix_tokens: 1_024,
            },
            None,
        ),
    ] {
        let (client, judge) = recording_judge(FrontierModelSpec {
            cache_model,
            ..spec()
        });
        let quote = quote_of(&client, &judge, "system", "brief").await;
        assert_eq!(quote.cache_ttl_ms, requested, "{cache_model:?}");
    }
}

/// **CONTROL.** The isolations a cache-aware check must not trade away: a
/// key of its own, and no block index borrowed from the conversation's
/// ledger, which names a block in a different prompt.
#[tokio::test]
async fn a_check_keeps_its_own_key_and_borrows_no_conversation_breakpoint() {
    let (client, judge) = recording_judge(spec());
    let quote = quote_of(&client, &judge, "system", "brief").await;

    assert_eq!(quote.prompt_cache_key, "acme/ada/main#validate");
    assert_eq!(
        quote.previous_breakpoint, None,
        "the conversation breakpoint does not belong to the judge prompt"
    );
    assert_eq!(
        quote.output_token_cap,
        Some(JudgeConfig::default().expected_output_tokens),
        "the checker's ceiling survives the cache work"
    );
    assert!(quote.tools.is_none());
}

/// An empty half names no boundary rather than one the client must refuse.
///
/// A boundary at `0` or at the end of the prompt is a
/// [`FrontierError::MalformedQuote`], so a rule that always split would
/// turn an empty brief into an abandoned check.
#[tokio::test]
async fn an_empty_prompt_half_names_no_boundary() {
    for (system_prompt, brief, boundaries) in [
        ("system", "brief", 1),
        ("", "brief", 0),
        ("system", "", 0),
        ("", "", 0),
    ] {
        let (client, judge) = recording_judge(spec());
        let quote = quote_of(&client, &judge, system_prompt, brief).await;
        assert_eq!(
            quote.segment_boundaries.len(),
            boundaries,
            "({system_prompt:?}, {brief:?})"
        );
        let segments = quote
            .segments()
            .unwrap_or_else(|error| panic!("({system_prompt:?}, {brief:?}): {error}"));
        assert_eq!(segments.concat(), quote.prompt);
    }
}

/// **CONTROL.** A check marks a block for caching, so what it reserves is
/// the cold write and never the plain input rate.
///
/// The direction is the whole point: reserving at the input rate and then
/// asking the provider for a charged write is a grant that cannot settle.
/// [`ProviderPricing::price`] takes the conservative branch when nothing
/// measured a write, which is what makes this hold — and what a second,
/// judge-local pricing rule would quietly undo.
#[tokio::test]
async fn a_check_reserves_the_write_premium_and_refuses_before_it_sends() {
    let ledger = Arc::new(MemorySpendLedger::new());
    let client = Arc::new(RecordingSilentClient::default());
    let judge = FleetJudge::new(
        Arc::clone(&client) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    )
    .with_spend_ledger(Arc::clone(&ledger) as Arc<dyn SpendLedger>);

    let brief = "x".repeat(4_000);
    let tokens = ByteTokenizer.encode(&format!("system\n\n{brief}")).len() as f64;
    let output = JudgeConfig::default().expected_output_tokens as f64;
    let card = spec().pricing;
    let per_mtok = 1e-6;
    let answer = output * card.output_per_mtok_usd * per_mtok;
    let input_only = tokens * card.input_per_mtok_usd * per_mtok + answer;
    let cold_write = tokens * card.cache_write_per_mtok_usd * per_mtok + answer;
    assert!(
        input_only < cold_write,
        "the fixture card must price a write above plain input, or this \
         asserts nothing"
    );

    let between = terms((input_only + cold_write) / 2.0);
    assert_eq!(
        judge
            .consult(&Check::nth(0).under(Some(&between)), "system", &brief)
            .await,
        Err(JudgeFailure::Unaffordable),
        "a ceiling that covers the prompt at the input rate but not at the \
         write premium must refuse the check"
    );
    assert!(
        client.seen.lock().expect("recording").is_empty(),
        "and refuse it before the socket, not after"
    );

    // The control: a ceiling that covers the premium makes the same check.
    let funded = terms(cold_write * 2.0);
    judge
        .consult(&Check::nth(1).under(Some(&funded)), "system", &brief)
        .await
        .expect("a funded membership gets its check");
    assert_eq!(client.seen.lock().expect("recording").len(), 1);
}

/// **CONTROL.** A card that prices no separate write bills the plain input
/// rate. A dialect with no write accounting must not be charged a premium
/// nobody published.
#[test]
fn a_target_that_prices_no_write_reserves_the_plain_input_rate() {
    let spec = FrontierModelSpec {
        pricing: ProviderPricing {
            cache_write_per_mtok_usd: 0.0,
            ..spec().pricing
        },
        ..spec()
    };
    let judge = FleetJudge::new(
        Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
        spec.clone(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    );
    let tokens = 1_000u64;
    let expected = tokens as f64 * spec.pricing.input_per_mtok_usd * 1e-6
        + JudgeConfig::default().expected_output_tokens as f64
            * spec.pricing.output_per_mtok_usd
            * 1e-6;

    assert!(
        (judge.estimated_cost_usd(tokens) - expected).abs() < 1e-12,
        "{} against {expected}",
        judge.estimated_cost_usd(tokens)
    );
}

/// A provider that streams an answer and never says what it billed.
///
/// The common case rather than an anomaly — a streaming OpenAI-compatible
/// endpoint sends no usage unless the request asked for it, and a gateway
/// in the path can drop it even when it did — and the one the judge's own
/// accounting has to fill in rather than book as free.
struct SilentClient;

#[async_trait]
impl FrontierClient for SilentClient {
    async fn execute(&self, _quote: &FrontierQuote) -> Result<FrontierStream, FrontierError> {
        Ok(futures::stream::iter([Ok(FrontierChunk::OutputText(
            r#"{"on_track":true,"confidence":0.9,"divergence":null,"missing_context":null}"#
                .to_string(),
        ))])
        .boxed())
    }
}

fn spec() -> FrontierModelSpec {
    FrontierModelSpec {
        provider: "anthropic".into(),
        model: "claude".into(),
        wire_protocol: WireProtocol::AnthropicMessages,
        cache_model: CacheModel::Deterministic { ttl_ms: 300_000 },
        pricing: ProviderPricing {
            input_per_mtok_usd: 3.0,
            cached_input_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            output_per_mtok_usd: 15.0,
        },
        quality_prior: 0.95,
        base_ttft_ms: 350.0,
        ttft_ms_per_uncached_token: 0.002,
    }
}

fn terms(limit_usd: f64) -> BudgetTerms {
    BudgetTerms {
        budget: Budget {
            limit_usd,
            window: BudgetWindow::Total,
            on_exhaustion: Exhaustion::degrade_with_overflow(),
            warn_at: DEFAULT_WARN_AT,
        },
        allocation: Allocation::Pooled,
    }
}

/// What one validated turn of a session hands the judge, owned so a test
/// can lend it out.
///
/// `n` is which validated turn of the session this is, and it moves the one
/// field a repeat has to move: a fresh id, so two checks can neither share a
/// hold nor have the second's settle deduplicated as a repeat of the first.
/// A fixture that reused one `Check` across several consults would be
/// modelling a single turn checked many times, which no engine does — and
/// would now be silently dropping every settle after the first.
struct Check {
    session_id: SessionId,
    principal: Principal,
    id: SideCallId,
}

impl Check {
    fn nth(n: u64) -> Self {
        Self {
            session_id: SessionId::new("acme/ada/main"),
            principal: Principal::new("acme", "ada"),
            id: SideCallId::new(format!("sc_{n}")),
        }
    }

    fn under<'a>(&'a self, budget: Option<&'a BudgetTerms>) -> SideCall<'a> {
        SideCall {
            session_id: &self.session_id,
            id: &self.id,
            principal: &self.principal,
            budget,
        }
    }
}

#[tokio::test]
async fn the_side_call_carries_its_own_cache_key_and_never_the_conversations() {
    let client = Arc::new(RecordingClient::default());
    let judge = FleetJudge::new(
        Arc::clone(&client) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    );
    let first = Check::nth(0);
    let second = Check::nth(1);

    judge
        .consult(&first.under(None), "system", "brief")
        .await
        .expect("the scripted client answers");
    // And again: the key is stable across validations, which is what lets
    // the judge's own prefix warm.
    judge
        .consult(&second.under(None), "system", "a later brief")
        .await
        .expect("the scripted client answers");

    let seen = client.seen.lock().expect("recording");
    let keys: Vec<&str> = seen
        .iter()
        .map(|quote| quote.prompt_cache_key.as_str())
        .collect();
    assert_eq!(keys, ["acme/ada/main#validate", "acme/ada/main#validate"]);
    // The control that makes the assertion above about isolation rather
    // than about a string: the conversation's own key is what the engine
    // sends, and it must not be what this sent.
    assert!(
        keys.iter().all(|key| *key != first.session_id.to_string()),
        "a judge prompt on the conversation's key cools the hit the router \
         just priced: {keys:?}"
    );
}

#[tokio::test]
async fn a_budget_with_no_room_skips_the_check_instead_of_failing_the_turn() {
    let client = Arc::new(RecordingClient::default());
    let ledger = Arc::new(MemorySpendLedger::new());
    let judge = FleetJudge::new(
        Arc::clone(&client) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    )
    .with_spend_ledger(Arc::clone(&ledger) as Arc<dyn SpendLedger>);

    // A limit far below the price of one check: 400 bytes of prompt on this
    // card is dollars, and the ceiling is a fraction of a cent.
    let brief = "x".repeat(400);
    let broke = terms(0.000_001);
    let refused = judge
        .consult(&Check::nth(0).under(Some(&broke)), "system", &brief)
        .await;
    assert_eq!(refused, Err(JudgeFailure::Unaffordable));
    assert!(
        client.seen.lock().expect("recording").is_empty(),
        "a check nobody can afford must cost the turn nothing at all, not a \
         round trip that is then thrown away"
    );
    assert_eq!(
        position(&ledger, &broke).await.held_usd,
        0.0,
        "and it must leave nothing behind: a refusal that stranded the \
         partial reservation it was refused on would tighten the ceiling \
         again on the next turn"
    );

    // The control: the identical check under a ceiling that covers it is
    // made, so the refusal above is about the budget and not about the
    // fixture.
    let funded = terms(100.0);
    judge
        .consult(&Check::nth(1).under(Some(&funded)), "system", &brief)
        .await
        .expect("a funded membership gets its check");
    assert_eq!(client.seen.lock().expect("recording").len(), 1);
}

#[tokio::test]
async fn a_provider_that_refuses_is_abandoned_against_its_own_target() {
    let client = Arc::new(RecordingClient {
        seen: Mutex::new(Vec::new()),
        fail: Some(FrontierError::Upstream("429".into())),
    });
    let judge = FleetJudge::new(
        client as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    );

    let failed = judge
        .consult(&Check::nth(0).under(None), "system", "brief")
        .await;
    assert_eq!(
        failed,
        Err(JudgeFailure::Abandoned {
            target: spec().target(),
            reason: SideCallAbandonReason::Refused,
        }),
        "a provider that answered and refused is not a provider nobody \
         could reach, and the two send an operator to different places"
    );
}

/// What the [`RecordingClient`] fixture's reported usage costs on
/// [`spec`]'s card: 900 input and 40 output tokens.
fn recorded_cost_usd() -> f64 {
    spec().pricing.price(&Usage {
        input_tokens: 900,
        cached_input_tokens: 0,
        cache_read_source: CacheReadSource::Provider,
        // `whole_response` reports none, which is what an adapted
        // non-streaming backend knows about a remote cache write.
        cache_write_tokens: 0,
        output_tokens: 40,
        reasoning_tokens: 0,
        accounting: Accounting::Reported,
    })
}

async fn position(ledger: &Arc<MemorySpendLedger>, terms: &BudgetTerms) -> Balance {
    ledger
        .balance(BalanceQuery {
            principal: Principal::new("acme", "ada"),
            terms: terms.clone(),
            now_ms: now_ms(),
        })
        .await
        .expect("the memory ledger answers")
}

fn judge_over(
    client: Arc<dyn FrontierClient>,
    ledger: &Arc<MemorySpendLedger>,
) -> FleetJudge<ByteTokenizer> {
    FleetJudge::new(
        client,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    )
    .with_spend_ledger(Arc::clone(ledger) as Arc<dyn SpendLedger>)
}

/// **What a check costs reaches the ledger, or no ceiling bounds it.**
///
/// The judge's dollars were folded into metrics and reported on the wire,
/// and committed nowhere: the only settle in the system prices a *turn's*
/// terminal event, and a side call is a separate model call with no
/// terminal event of its own. So `measured_usd` moved and `committed_usd`
/// did not, and the pre-flight budget read — which asks about one check at
/// a time — could never see the spend of the checks before it.
#[tokio::test]
async fn what_a_check_spends_is_committed_to_the_payers_ledger() {
    let ledger = Arc::new(MemorySpendLedger::new());
    let judge = judge_over(Arc::new(RecordingClient::default()), &ledger);
    let terms = terms(100.0);

    judge
        .consult(&Check::nth(0).under(Some(&terms)), "system", "brief")
        .await
        .expect("a funded membership gets its check");

    let after = position(&ledger, &terms).await;
    assert!(
        (after.committed_usd - recorded_cost_usd()).abs() < 1e-12,
        "the check's own reported usage, priced on the judge's card, is what \
         the ledger must hold: {after:?} against {}",
        recorded_cost_usd()
    );
    assert_eq!(
        after.held_usd, 0.0,
        "the hold is closed by the settle, not left to lapse on a TTL — a \
         check that stranded a reservation every validation would be worse \
         than the overspend it prevents"
    );
}

/// The consequence, and the assertion the whole finding is about: once a
/// membership's checks have spent its ceiling, the next check is refused.
#[tokio::test]
async fn checks_stop_once_their_own_spend_has_reached_the_ceiling() {
    let ledger = Arc::new(MemorySpendLedger::new());
    let judge = judge_over(Arc::new(RecordingClient::default()), &ledger);
    // A ceiling a few checks wide: each one bills ~$0.0033, and each one's
    // *estimate* is under a cent on its own, so nothing but committed spend
    // can stop the tenth.
    let ceiling = terms(0.01);

    let mut allowed = 0;
    for turn in 0..10 {
        if judge
            .consult(&Check::nth(turn).under(Some(&ceiling)), "system", "brief")
            .await
            .is_ok()
        {
            allowed += 1;
        }
    }
    assert!(
        (1..10).contains(&allowed),
        "a $0.01 ceiling must stop granting checks once real judge spend has \
         exceeded it, but {allowed} of 10 checks were allowed"
    );
    let after = position(&ledger, &ceiling).await;
    assert!(
        after.project_remaining_usd < recorded_cost_usd(),
        "and it must be the ceiling that stopped them: {after:?}"
    );

    // The control: the identical run under a ceiling that covers it makes
    // every check, so the refusals above are about the money and not about
    // the fixture running out of scripted answers.
    let roomy = terms(100.0);
    let funded = judge_over(
        Arc::new(RecordingClient::default()),
        &Arc::new(MemorySpendLedger::new()),
    );
    for turn in 0..10 {
        funded
            .consult(&Check::nth(turn).under(Some(&roomy)), "system", "brief")
            .await
            .expect("a funded membership is checked every time");
    }
}

/// A check that was made and produced nothing must not hold money either.
#[tokio::test]
async fn an_abandoned_check_gives_its_hold_back() {
    let ledger = Arc::new(MemorySpendLedger::new());
    let judge = judge_over(
        Arc::new(RecordingClient {
            seen: Mutex::new(Vec::new()),
            fail: Some(FrontierError::Upstream("429".into())),
        }),
        &ledger,
    );
    let funded = terms(100.0);

    let failed = judge
        .consult(&Check::nth(0).under(Some(&funded)), "system", "brief")
        .await;
    assert!(matches!(failed, Err(JudgeFailure::Abandoned { .. })));

    let after = position(&ledger, &funded).await;
    assert_eq!(
        after.held_usd, 0.0,
        "a judge that is refusing every call would otherwise strand a hold \
         per turn for a TTL, which is the failure a hold on this path was \
         once rejected for: {after:?}"
    );
    assert_eq!(
        after.committed_usd, 0.0,
        "and nothing is booked, because nothing this deployment can price \
         was produced"
    );

    // The half that makes both assertions above about *releasing* rather
    // than about never holding at all: a second check under a ceiling one
    // and a half checks wide is still made. Had the abandoned call kept its
    // reservation, half a check's room would be left and this would come
    // back `Unaffordable` — a judge that is refusing every call would
    // tighten its own budget one dead check at a time.
    let estimate = judge
        .estimated_cost_usd(judge.counted_input_tokens(&PreparedPrompt::new("system", "brief")));
    let narrow = terms(estimate * 1.5);
    assert!(
        matches!(
            judge
                .consult(&Check::nth(1).under(Some(&narrow)), "system", "brief")
                .await,
            Err(JudgeFailure::Abandoned { .. })
        ),
        "the second check must reach the provider and fail there, not be \
         refused by money the first check never gave back: {:?}",
        position(&ledger, &narrow).await
    );
}

/// A stream that ends without an accounting chunk is booked at what we
/// sent, never at zero.
///
/// The input axis dominates a check's cost — a multi-kilobyte brief against
/// a four-field verdict — and it is the one axis the fallback used to
/// hardcode to zero, while the estimate the budget question was asked with,
/// three functions up the same file, already counted it exactly.
#[tokio::test]
async fn a_check_nobody_billed_for_is_estimated_from_what_we_sent() {
    let judge = FleetJudge::new(
        Arc::new(SilentClient) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    );
    let system_prompt = "system";
    // A brief that dwarfs the verdict, which is the realistic shape and the
    // reason a zero on this axis is not a rounding error.
    let brief = "x".repeat(4_000);

    let answer = judge
        .consult(&Check::nth(0).under(None), system_prompt, &brief)
        .await
        .expect("a stream that ended is an answer, not a failure");

    assert_eq!(
        answer.usage.input_tokens,
        ByteTokenizer
            .encode(&PreparedPrompt::new(system_prompt, &brief).text)
            .len() as u64,
        "the prompt is what we tokenized and sent, so it is a count and not \
         a guess — the joined string, because that is what went out"
    );
    assert!(answer.usage.output_tokens > 0);
    assert_eq!(
        answer.usage.accounting,
        Accounting::Estimated,
        "measured and estimated never merge: a filled-in gap must stay \
         distinguishable from a provider's own number"
    );

    // The control: the identical call over a provider that *does* account
    // for itself carries the provider's numbers, stamped as reported.
    let reporting = FleetJudge::new(
        Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig::default(),
    );
    let reported = reporting
        .consult(&Check::nth(1).under(None), system_prompt, &brief)
        .await
        .expect("the scripted client answers");
    assert_eq!(reported.usage.accounting, Accounting::Reported);
    assert_eq!(
        (reported.usage.input_tokens, reported.usage.output_tokens),
        (900, 40)
    );
}

#[test]
fn a_deadline_fraction_at_or_past_one_is_clamped_below_the_turns() {
    let judge = FleetJudge::new(
        Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig {
            deadline_fraction: 4.0,
            ..JudgeConfig::default()
        },
    );
    assert!(
        judge.config.deadline_fraction < 1.0,
        "the checker must never break the checked, and a fraction is the \
         only thing standing between a hung judge and the turn's whole budget"
    );
    // The control: a sane fraction is left exactly as written.
    let judge = FleetJudge::new(
        Arc::new(RecordingClient::default()) as Arc<dyn FrontierClient>,
        spec(),
        ByteTokenizer,
        120_000,
        JudgeConfig {
            deadline_fraction: 0.25,
            ..JudgeConfig::default()
        },
    );
    assert_eq!(judge.config.deadline_fraction, 0.25);
}
