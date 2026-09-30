// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! C2's live evidence, and the fake-server proof that the probe itself is
//! honest.
//!
//! `PLAN-cache-affinity.md` C2 places a second `cache_control` marker back
//! where the previous request to a target wrote its entry, because Anthropic
//! looks at most twenty block positions back from a marker. The unit tests
//! prove that arithmetic against `body()`. Whether the provider agrees is
//! something only a real two-turn session can observe.
//!
//! **One driver, two backends.** [`probe::two_turn_probe`] drives the same
//! engine, the same admission and the same budget whichever upstream is
//! behind it. The loopback tests below and the feature-gated live test differ
//! in the base URL and in whether the key is real — nothing else — so what
//! the offline suite asserts is a property of the code the live run will
//! execute.
//!
//! **Both counters, both turns.** A zero read on turn two settles nothing by
//! itself: turn one may never have touched the cache, or it may have read or
//! written something turn two could not reach. [`probe::ProbeReport`] keeps
//! `cache_write_tokens` and `cached_input_tokens` for each turn so the two
//! are never confused — a zero read with no read or write observed on turn
//! one is an inconclusive run, not evidence against the block arithmetic.

use std::sync::Arc;

use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::ProviderPricing;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::WireProtocol;

use crate::engine::{PROVIDER, admission, catalog, engine, priced, spec_with};
use crate::fake_upstream::{blocks, gateway_provider, loopback_provider, markers, spawn, spawn_at};
use crate::preflight::probe_inputs;
use crate::probe::{ProbeReport, TurnCache, input, two_turn_probe};

mod common;
#[path = "cache_probe/engine.rs"]
mod engine;
#[path = "cache_probe/fake_upstream.rs"]
mod fake_upstream;
#[cfg(feature = "e2e-frontier")]
#[path = "cache_probe/live.rs"]
mod live;
#[path = "cache_probe/preflight.rs"]
mod preflight;
#[path = "cache_probe/probe.rs"]
mod probe;

/// The floor this fixture holds its own marked prefix to, in words.
///
/// A bound on the fixture, not a claim about any provider: minimum cacheable
/// prefixes are per model and are counted in that model's tokens, which this
/// deployment does not tokenize with. Words are a coarse proxy chosen so the
/// fixture is generously long — overshooting costs a bigger request, and
/// undershooting costs a zero read that reads like a finding. Whether a
/// *particular* model's minimum is cleared is a question for the live
/// preflight, against the model the operator pins.
const MIN_CACHED_WORDS: usize = 8_192;

const FIXTURE_KEY: &str = "sk-ant-api03-PROBEZZZZ-fixture";

/// The probe dispatches twice and no more.
#[tokio::test]
async fn the_probe_sends_exactly_two_requests() {
    let (base, upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &loopback_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    two_turn_probe(&engine, &store, &admission(10.0, PROVIDER, FIXTURE_KEY))
        .await
        .expect("the turns are served");

    assert_eq!(
        upstream.calls(),
        2,
        "the driver is hard-bounded at two dispatches, because the live form \
         of it spends money"
    );
}

/// The second request marks the first one's block and resends its bytes.
#[tokio::test]
async fn the_second_request_reuses_the_old_breakpoint_over_an_unchanged_prefix() {
    let (base, upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &loopback_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    two_turn_probe(&engine, &store, &admission(10.0, PROVIDER, FIXTURE_KEY))
        .await
        .expect("the turns are served");

    let first = upstream.body(0);
    let second = upstream.body(1);
    let (first_blocks, second_blocks) = (blocks(&first), blocks(&second));
    let first_markers = markers(&first);
    let second_markers = markers(&second);

    assert_eq!(
        first_markers.len(),
        1,
        "a first turn has one stable prefix to mark"
    );
    assert_eq!(
        second_markers.len(),
        2,
        "an append this long puts the penultimate marker out of the previous \
         write's reach, so the old block is marked as well: {second_markers:?}"
    );
    assert!(
        second_markers.contains(&first_markers[0]),
        "the earlier marker must sit on the block the first request wrote, \
         not merely somewhere earlier: first={first_markers:?} \
         second={second_markers:?}"
    );
    assert_eq!(
        &second_blocks[..first_blocks.len()],
        &first_blocks[..],
        "the prefix the second request resends must be byte-identical, or the \
         provider could not match it however it was marked"
    );
    // The response replay adds a block, so two markers alone do not prove
    // that the requested twenty new items reached the provider.
    let appended_on_the_wire = second_blocks
        .iter()
        .filter(|text| text.contains("appended item "))
        .count();
    assert!(
        appended_on_the_wire >= 20,
        "the probe brief requires the second turn to append at least twenty \
         items; the wire carried {appended_on_the_wire}"
    );
}

/// Both turns' counters come off the log, and stay apart.
#[tokio::test]
async fn both_turns_report_their_own_write_and_read() {
    let (base, _upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &loopback_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    let report = two_turn_probe(&engine, &store, &admission(10.0, PROVIDER, FIXTURE_KEY))
        .await
        .expect("the turns are served");

    assert_eq!(report.first.written, 5_000, "the first turn wrote an entry");
    assert_eq!(report.first.read, 0, "and read none, being first");
    assert_eq!(report.second.read, 4_800, "the second turn read it back");
    assert!(report.render().starts_with("READ"), "{}", report.render());
}

/// The verdict reports what was observed, and never why.
///
/// A read on turn two is a read; which write filled the entry is not in the
/// usage object, and a probe that said "the entry the first turn wrote" would
/// be asserting causation from two counters that cannot carry it. The first
/// turn's write stays in the line either way, because whether this run created
/// the entry it read is exactly what a reader needs and exactly what the
/// verdict must not decide for them.
#[test]
fn the_verdict_states_observations_rather_than_causes() {
    let turn = |read, written| TurnCache {
        read,
        written,
        input: 100,
        reported: true,
    };

    // A cold first turn that wrote, then a read. The strongest shape, and even
    // here the line says what was read, not what filled it.
    let cold_then_read = ProbeReport {
        first: turn(0, 5_000),
        second: turn(4_800, 0),
    };
    let rendered = cold_then_read.render();
    assert!(rendered.starts_with("READ"), "{rendered}");
    assert!(
        !rendered.contains("wrote it") && !rendered.contains("the entry the first"),
        "no causal claim belongs in the verdict: {rendered}"
    );

    // **A first turn that was already warm**: it read and wrote nothing. The
    // second turn's read is a real observation; what it is not is evidence
    // that this run's write produced it, and the line has to say both.
    let warm_already = ProbeReport {
        first: turn(4_000, 0),
        second: turn(4_800, 0),
    };
    let rendered = warm_already.render();
    assert!(
        rendered.starts_with("READ"),
        "a read observed after a warm first turn is still a read: {rendered}"
    );
    assert!(
        rendered.contains("no write was observed on turn 1"),
        "and the absent write stays explicit: {rendered}"
    );

    // Wrote and then read nothing back: the result that matters most.
    let wrote_but_missed = ProbeReport {
        first: turn(0, 5_000),
        second: turn(0, 0),
    };
    assert!(
        wrote_but_missed.render().starts_with("NO READ"),
        "{}",
        wrote_but_missed.render()
    );

    // Neither wrote nor read: nothing happened that bears on the question.
    let nothing = ProbeReport {
        first: turn(0, 0),
        second: turn(0, 0),
    };
    assert!(
        nothing.render().starts_with("INCONCLUSIVE"),
        "{}",
        nothing.render()
    );

    // Counts the provider never sent are not observations of zero.
    let estimated = ProbeReport {
        first: TurnCache {
            reported: false,
            ..turn(0, 5_000)
        },
        second: TurnCache {
            reported: false,
            ..turn(0, 0)
        },
    };
    assert!(
        estimated.render().starts_with("UNREPORTED"),
        "{}",
        estimated.render()
    );

    // --- the three mixed shapes ---------------------------------------------

    // **No write observed, and a read.** Where the entry came from is not in
    // the usage object: the provider may have written one and reported the
    // creation as zero, or it may have been there already. The line reports
    // the absent write and stops.
    let read_without_a_write = ProbeReport {
        first: turn(0, 0),
        second: turn(4_800, 0),
    };
    let rendered = read_without_a_write.render();
    assert!(rendered.starts_with("READ"), "{rendered}");
    assert!(
        !rendered.contains("predates"),
        "the run cannot date an entry it did not watch being written: {rendered}"
    );
    assert!(
        rendered.contains("no write"),
        "and the absent write stays explicit: {rendered}"
    );

    // **A warm first turn and no second read.** Cache activity happened this
    // run — a read on turn one — so calling it inconclusive says something
    // false about what was seen.
    let warm_then_nothing = ProbeReport {
        first: turn(4_000, 0),
        second: turn(0, 0),
    };
    let rendered = warm_then_nothing.render();
    assert!(
        rendered.starts_with("NO READ"),
        "turn one showed cache activity, so this is a missing read rather than \
         an absence of evidence: {rendered}"
    );

    // **One turn unreported, one not.** The missing provenance is turn one's;
    // turn two's counters came from the provider and remain an observation.
    let half_reported = ProbeReport {
        first: TurnCache {
            reported: false,
            ..turn(0, 0)
        },
        second: turn(4_800, 0),
    };
    let rendered = half_reported.render();
    assert!(rendered.starts_with("UNREPORTED"), "{rendered}");
    assert!(
        rendered.contains("turn 1"),
        "the verdict names the turn whose provenance is missing: {rendered}"
    );
    assert!(
        !rendered.contains("no figure"),
        "and does not erase the turn that did report: {rendered}"
    );
}

/// The transport is built from the provider definition, not from its base URL.
///
/// A catalog whose `base_url` already ends in `/v1` and whose route is
/// `/messages` is the ordinary gateway shape, and a client that kept its own
/// default path would post to `/v1/v1/messages`. The same definition carries
/// the auth spelling — OpenRouter's `/messages` route refuses an `x-api-key`
/// and Anthropic's refuses a bearer — and the static headers a gateway asks
/// for. `main`'s `messages_client` reads all three; a probe that read none of
/// them would pass here and 404 or 401 on the one run that matters.
#[tokio::test]
async fn the_transport_honours_the_configured_route_auth_and_headers() {
    // Mounted where the *definition* says, which is also where a client that
    // ignored it would not post.
    let (base, upstream) = spawn_at("/v1", "/messages", vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &gateway_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    two_turn_probe(&engine, &store, &admission(10.0, PROVIDER, FIXTURE_KEY))
        .await
        .expect("the turns are served");

    assert_eq!(
        upstream.calls(),
        2,
        "both turns reached the configured path; a default path would have \
         missed this mount entirely"
    );
    let arrived = upstream.headers();
    assert!(
        arrived.contains(&format!("authorization: Bearer {FIXTURE_KEY}")),
        "the definition says bearer, so the key rides there: {arrived}"
    );
    assert!(
        !arrived.contains("x-api-key"),
        "and not in the header this provider would ignore: {arrived}"
    );
    assert!(
        arrived.contains("x-probe-gateway: roundhouse-cache-probe"),
        "a gateway's static headers travel with every request: {arrived}"
    );
}

/// A cap the driver was handed is a cap the driver enforces.
///
/// Through [`two_turn_probe`] rather than around it: the live run reaches the
/// socket through that function and nothing else, so a ceiling proven against a
/// hand-rolled call proves nothing about the path that spends money.
#[tokio::test]
async fn a_cap_the_driver_cannot_afford_stops_it_before_the_socket() {
    let (base, upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &loopback_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    let refused = two_turn_probe(
        &engine,
        &store,
        // Below any hold this catalog can take.
        &admission(0.000_000_001, PROVIDER, FIXTURE_KEY),
    )
    .await;

    assert!(
        refused.is_err(),
        "a project that cannot afford the first turn does not get served"
    );
    assert_eq!(
        upstream.calls(),
        0,
        "and the refusal lands before the socket, or the cap is decoration"
    );
}

/// What the live preflight refuses, asserted without touching the environment.
///
/// The checks are a pure function of the catalog entry and the ceiling, so they
/// are tested as one — `preflight` reads the three variables and calls this, and
/// setting variables in a test binary to exercise them would be a process-wide
/// mutation for no coverage this does not already give.
#[test]
fn the_preflight_refuses_every_input_that_would_waste_a_live_run() {
    let ok = |pricing| probe_inputs(1.0, &spec_with(pricing, WireProtocol::AnthropicMessages));
    assert!(
        ok(priced()).is_ok(),
        "the shipped fixture shape is runnable"
    );

    // The ceiling.
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let refused = probe_inputs(bad, &spec_with(priced(), WireProtocol::AnthropicMessages));
        assert!(
            refused.is_err(),
            "a ceiling of {bad} bounds nothing, so the run must not start"
        );
    }

    // Every price this dialect's cache economics touch, one at a time — a
    // placeholder in any of them means the hold, the discount or the write
    // premium is fictional.
    for (field, pricing) in [
        (
            "input",
            ProviderPricing {
                input_per_mtok_usd: 0.0,
                ..priced()
            },
        ),
        (
            "output",
            ProviderPricing {
                output_per_mtok_usd: 0.0,
                ..priced()
            },
        ),
        (
            "cache_write",
            ProviderPricing {
                cache_write_per_mtok_usd: 0.0,
                ..priced()
            },
        ),
        (
            "cached_input",
            ProviderPricing {
                cached_input_per_mtok_usd: 0.0,
                ..priced()
            },
        ),
    ] {
        assert!(
            ok(pricing).is_err(),
            "a zero {field} price is a placeholder, and this probe is priced"
        );
    }

    // And the dialect, because `cache_control` is this wire's vocabulary.
    assert!(
        probe_inputs(1.0, &spec_with(priced(), WireProtocol::OpenAiResponses)).is_err(),
        "another dialect places no marker for this probe to measure"
    );
}

/// The prefix the first marker covers is big enough to be worth caching.
///
/// Providers only write an entry above a minimum prefix length, so a probe
/// whose marked prefix falls below it reports a zero read for a reason that has
/// nothing to do with the second marker — and the run looks like evidence.
/// Counted in whitespace-separated words rather than in `ByteTokenizer`'s
/// bytes, because the threshold that matters upstream is a token count and
/// words are the closest honest proxy a test can assert without pinning a
/// tokenizer this deployment does not use.
#[tokio::test]
async fn the_marked_prefix_is_long_enough_to_be_worth_caching() {
    let (base, upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &loopback_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    two_turn_probe(&engine, &store, &admission(10.0, PROVIDER, FIXTURE_KEY))
        .await
        .expect("the turns are served");

    let first = upstream.body(0);
    let marker = markers(&first)[0];
    let cached_words: usize = blocks(&first)[..=marker]
        .iter()
        .map(|text| text.split_whitespace().count())
        .sum();
    assert!(
        cached_words >= MIN_CACHED_WORDS,
        "the marked prefix is {cached_words} words; below the model's own minimum \
         a zero read says nothing about the marker"
    );
}

/// Two runs of the probe cannot share a cache entry.
///
/// A prefix identical to yesterday's run is already warm upstream, so the
/// second turn would read a hit this run did not write and the evidence would
/// be somebody else's. The nonce sits *early* in the first item, before
/// anything the runs share, so the divergence is inside the first block rather
/// than at the end of a long common prefix.
#[tokio::test]
async fn two_runs_share_no_prefix_while_one_run_repeats_its_own() {
    let mut firsts = Vec::new();
    for _ in 0..2 {
        let (base, upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
        let store = Arc::new(MemoryStore::new());
        let engine = engine(
            &loopback_provider(&base),
            PROVIDER,
            catalog(priced()),
            Arc::clone(&store),
        );
        two_turn_probe(&engine, &store, &admission(10.0, PROVIDER, FIXTURE_KEY))
            .await
            .expect("the turns are served");

        // Within one run the prefix is repeated byte for byte — the property
        // the other test asserts, restated here so this one cannot pass by
        // making every request unique.
        let (first, second) = (upstream.body(0), upstream.body(1));
        let first_blocks = blocks(&first);
        assert_eq!(
            &blocks(&second)[..first_blocks.len()],
            &first_blocks[..],
            "one run resends its own prefix unchanged"
        );
        firsts.push(first_blocks[0].clone());
    }

    assert_ne!(
        firsts[0], firsts[1],
        "two runs must diverge in the first block, or the second run reads an \
         entry the first one wrote"
    );
}

/// A budget that cannot cover one turn spends no network call.
///
/// The cap is what stands between a probe and a surprise bill, so the
/// assertion is about the socket rather than about the error: a refusal that
/// still dispatched would be a cap in name only.
#[tokio::test]
async fn a_budget_that_refuses_the_first_grant_makes_no_http_call() {
    let (base, upstream) = spawn(vec![(0, 5_000), (4_800, 0)]).await;
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        &loopback_provider(&base),
        PROVIDER,
        catalog(priced()),
        Arc::clone(&store),
    );

    let session_id = SessionId::generate();
    engine
        .create_session(&session_id)
        .await
        .expect("the session opens");
    let refused = engine
        .run_turn(
            &session_id,
            TurnId::new("probe-1"),
            input(vec![Item::user_text("hello")]),
            // Below any hold this catalog can take.
            &admission(0.000_000_001, PROVIDER, FIXTURE_KEY),
        )
        .await;

    assert!(
        refused.is_err(),
        "a project that cannot afford the turn and refuses on exhaustion does \
         not get served"
    );
    assert_eq!(
        upstream.calls(),
        0,
        "and the refusal happens before the socket, or the cap is decoration"
    );
}
