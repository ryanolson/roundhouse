// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
// The log fixtures live with the fold they build logs for; see
// `fold::tests`. One builder means one clock, so a test that compares a
// window across two logs is asserting about the fold rather than about two
// fixtures that happened to agree.
use crate::control::PrincipalKey;
use crate::event::{Accounting, IncompleteReason, SessionEventKind, Usage};
use crate::ids::{ResponseId, TurnId};
use crate::metrics::fold::tests::{LogBuilder, candidate, frontier, local, principal, usage};
use crate::routing::{DecisionRecord, LocalCapacityPrice, ProviderPricing};

const HOSTED: ProviderPricing = ProviderPricing {
    input_per_mtok_usd: 3.0,
    cached_input_per_mtok_usd: 0.3,
    cache_write_per_mtok_usd: 3.75,
    output_per_mtok_usd: 15.0,
};

pub(super) fn config() -> MetricsConfig {
    MetricsConfig::new(
        ShadowPricing::new(vec![ReferenceModel {
            provider: "anthropic".into(),
            model: "claude".into(),
            pricing: HOSTED,
            quality_prior: 0.6,
        }])
        .declare("llama", "anthropic", "claude", "matched on our eval suite"),
    )
    .with_default_local_quality(0.6)
}

pub(super) fn snapshot(fold: &MetricsFold) -> MetricsSnapshot {
    MetricsSnapshot::build(fold, Scope::Deployment, &config(), 9_999)
}

#[test]
fn a_replayed_log_folds_exactly_once() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 8_000, 500, 0),
    );

    let mut fold = MetricsFold::new();
    assert_eq!(fold.extend(log.events()), log.events().len());
    let once = snapshot(&fold);

    // The same events again — a live feed and a rebuild overlapping, which
    // is the normal case after a restart.
    assert_eq!(fold.extend(log.events()), 0, "no event should be new twice");
    let twice = snapshot(&fold);

    assert_eq!(once.calls, twice.calls);
    assert_eq!(once.tokens, twice.tokens);
    assert_eq!(
        once.savings.frontier_spend_usd,
        twice.savings.frontier_spend_usd
    );
}

#[test]
fn a_rebuild_from_the_log_matches_a_live_feed() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 8_000, 500, 120),
    );
    log.turn(
        "r2",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.042)],
        usage(12_000, 9_000, 600, 0),
    );

    // Live: one event at a time, as the engine appends them.
    let mut live = MetricsFold::new();
    for event in log.events() {
        live.apply(event);
    }
    // Rebuild: the whole log in one sweep, as a restarted process reads it.
    let mut rebuilt = MetricsFold::new();
    rebuilt.extend(log.events());

    let live = snapshot(&live);
    let rebuilt = snapshot(&rebuilt);
    assert_eq!(live.tokens, rebuilt.tokens);
    assert_eq!(live.calls, rebuilt.calls);
    assert_eq!(live.savings.total_usd, rebuilt.savings.total_usd);
    assert_eq!(live.models.len(), rebuilt.models.len());
}

#[test]
fn local_and_frontier_are_grouped_on_both_axes() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 0, 500, 0),
    );
    log.turn("r2", local("llama"), vec![], usage(20_000, 15_000, 800, 0));

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let snapshot = snapshot(&fold);

    assert_eq!(snapshot.models.len(), 2);
    assert_eq!(snapshot.serving_modes.len(), 2);

    let local_mode = snapshot
        .serving_modes
        .iter()
        .find(|m| m.mode == ServingMode::Local)
        .unwrap();
    assert_eq!(local_mode.totals.tokens.input, 20_000);
    assert_eq!(local_mode.totals.billed_usd, 0.0, "local bills nothing");
    assert!(local_mode.totals.shadow_usd > 0.0, "local is shadow-priced");

    // Local traffic is grouped under the fleet, not under a vendor.
    let local_row = snapshot
        .models
        .iter()
        .find(|m| m.mode() == ServingMode::Local)
        .unwrap();
    assert_eq!(local_row.provider, LOCAL_PROVIDER);
}

#[test]
fn savings_separate_measured_discounts_from_counterfactual_routing() {
    let mut log = LogBuilder::new("s1");
    // A hosted call with a warm cache: a real, measured discount.
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(100_000, 90_000, 1_000, 0),
    );
    // A local call: no money spent, so the saving is a counterfactual.
    log.turn(
        "r2",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(100_000, 90_000, 1_000, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let snapshot = snapshot(&fold);

    // 90k cached tokens at (3.75 write - 0.30 cached) per Mtok.
    let expected_cache = 90_000.0 * (3.75 - 0.3) * 1e-6;
    assert!((snapshot.savings.cache_savings_usd - expected_cache).abs() < 1e-12);

    // The local turn priced on its correlary, carrying its cache ratio.
    let expected_shadow = 10_000.0 * 3.75e-6 + 90_000.0 * 0.3e-6 + 1_000.0 * 15.0e-6;
    assert!((snapshot.savings.routing_savings_usd - expected_shadow).abs() < 1e-12);

    assert!(
        (snapshot.savings.total_usd
            - (snapshot.savings.cache_savings_usd + snapshot.savings.routing_savings_usd))
            .abs()
            < 1e-12
    );
    // The router's own quote for the road not taken, kept apart from the total.
    assert!((snapshot.savings.routing_savings_at_decision_usd - 0.05).abs() < 1e-12);
    assert!(snapshot.savings.frontier_spend_usd > 0.0);
}

/// M10 review G11: the provider's own figure reaches the report, at every
/// scope, and changes none of the figures the savings claim is computed
/// from.
///
/// The second half is the load-bearing one. `admin_api::reconciliation`
/// publishes `drift_usd = committed_usd - measured_usd`, and `measured_usd`
/// is `frontier_spend_usd` — so proving that number is byte-identical with
/// and without a provider-reported price is proving the sidecar cannot
/// reach drift. Asserted here rather than in the view because this is where
/// the arithmetic is; the view only formats it.
#[test]
fn a_provider_reported_price_is_published_beside_our_own_and_added_into_none_of_it() {
    let mut quiet = LogBuilder::new("s1");
    quiet.created(Some(principal("acme", "ada")));
    quiet.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 8_000, 500, 0),
    );
    let mut quiet_fold = MetricsFold::new();
    quiet_fold.extend(quiet.events());
    let quiet_snapshot = snapshot(&quiet_fold);

    let mut loud = LogBuilder::new("s1");
    loud.created(Some(principal("acme", "ada")));
    loud.turn_costing(
        "r1",
        frontier("anthropic", "claude"),
        usage(10_000, 8_000, 500, 0),
        0.00421,
    );
    let mut loud_fold = MetricsFold::new();
    loud_fold.extend(loud.events());
    let loud_snapshot = snapshot(&loud_fold);

    assert_eq!(
        quiet_snapshot.savings.provider_reported_usd, None,
        "a provider that says nothing leaves no figure, not a confident zero"
    );
    assert_eq!(
        loud_snapshot.savings.provider_reported_usd,
        Some(0.00421),
        "the deployment scope sums it out of the rows the same way every \
         other figure is summed"
    );

    // The invariant the reconciliation view's drift column depends on.
    assert_eq!(
        loud_snapshot.savings.frontier_spend_usd, quiet_snapshot.savings.frontier_spend_usd,
        "an upstream's own price must not move what we priced from the \
         catalog; drift is committed minus this, and a sidecar inside it \
         would make the cross-check a self-check"
    );
    assert_eq!(
        loud_snapshot.savings.total_usd,
        quiet_snapshot.savings.total_usd
    );
    assert_eq!(
        loud_snapshot.savings.cache_savings_usd,
        quiet_snapshot.savings.cache_savings_usd
    );

    // And the principal scope, which is the one the per-member rows of the
    // reconciliation view read. A figure that summed only at deployment
    // scope would read zero on every member row with nothing red.
    let mine = MetricsSnapshot::build(
        &loud_fold,
        Scope::Principal(&PrincipalKey::from(&principal("acme", "ada"))),
        &config(),
        9_999,
    );
    assert_eq!(mine.savings.provider_reported_usd, Some(0.00421));
}

#[test]
fn an_unreported_call_is_marked_rather_than_counted_as_free() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        Usage {
            accounting: Accounting::Estimated,
            ..usage(10_000, 0, 400, 0)
        },
    );
    log.turn(
        "r2",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 0, 400, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let snapshot = snapshot(&fold);

    assert_eq!(snapshot.coverage.calls, 2);
    assert_eq!(snapshot.coverage.estimated_calls, 1);
    assert!((snapshot.coverage_fraction - 0.5).abs() < 1e-12);
}

#[test]
fn a_dispatch_that_never_reached_the_provider_is_not_a_call() {
    let mut log = LogBuilder::new("s1");
    let response_id = ResponseId::new("r1");
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: response_id.clone(),
    });
    log.push(SessionEventKind::Routed {
        response_id: response_id.clone(),
        decision: DecisionRecord {
            cache_context_unverified: false,
            block_marker: None,
            selection: None,
            local_quote_skipped: None,
            chosen: frontier("anthropic", "claude"),
            rationale: "test".into(),
            policy: "test".into(),
            isl_tokens: 10_000,
            expected_prefill_tokens: 10_000.0,
            expected_cost_usd: 0.03,
            considered: vec![],
            turn_policy_digest: String::new(),
            budget_state: Default::default(),
            rate_card: None,
            payer: Default::default(),
            billing: Default::default(),
            budget_draw: None,
            withheld_providers: Vec::new(),
            declared_baseline: None,
            attempts: Vec::new(),
        },
    });
    // Failed before anything was sent: empty usage is the engine's way of
    // saying the prompt never reached the provider.
    log.push(SessionEventKind::ResponseIncomplete {
        response_id,
        reason: IncompleteReason::UpstreamError,
        usage: Usage::default(),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let snapshot = snapshot(&fold);

    assert_eq!(
        snapshot.calls, 0,
        "a turn that never dispatched is not a call"
    );
    assert_eq!(snapshot.turns, 1, "but it was still a turn");
    assert_eq!(snapshot.tokens.total, 0);
}

#[test]
fn an_incomplete_that_burned_tokens_is_still_billed() {
    let mut log = LogBuilder::new("s1");
    let response_id = ResponseId::new("r1");
    log.push(SessionEventKind::TurnStarted {
        turn_id: TurnId::new("t1"),
        response_id: response_id.clone(),
    });
    log.push(SessionEventKind::Routed {
        response_id: response_id.clone(),
        decision: DecisionRecord {
            cache_context_unverified: false,
            block_marker: None,
            selection: None,
            local_quote_skipped: None,
            chosen: frontier("anthropic", "claude"),
            rationale: "test".into(),
            policy: "test".into(),
            isl_tokens: 10_000,
            expected_prefill_tokens: 10_000.0,
            expected_cost_usd: 0.03,
            considered: vec![],
            turn_policy_digest: String::new(),
            budget_state: Default::default(),
            rate_card: None,
            payer: Default::default(),
            billing: Default::default(),
            budget_draw: None,
            withheld_providers: Vec::new(),
            declared_baseline: None,
            attempts: Vec::new(),
        },
    });
    log.push(SessionEventKind::ResponseIncomplete {
        response_id,
        reason: IncompleteReason::UpstreamError,
        usage: usage(10_000, 0, 0, 0),
        terminal_attempt: None,
    });

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let snapshot = snapshot(&fold);

    assert_eq!(snapshot.calls, 1);
    assert!(
        snapshot.savings.frontier_spend_usd > 0.0,
        "a prefill we were billed for is spend even though the answer never came"
    );
}

#[test]
fn sessions_are_counted_across_separate_logs() {
    let mut a = LogBuilder::new("s1");
    a.turn("r1", local("llama"), vec![], usage(1_000, 0, 100, 0));
    let mut b = LogBuilder::new("s2");
    b.turn("r2", local("llama"), vec![], usage(1_000, 0, 100, 0));

    let mut fold = MetricsFold::new();
    fold.extend(a.events());
    fold.extend(b.events());
    let snapshot = snapshot(&fold);

    assert_eq!(snapshot.sessions, 2);
    assert_eq!(snapshot.calls, 2);
    assert_eq!(
        snapshot.models.len(),
        1,
        "the same model across two sessions is one row"
    );
}

#[test]
fn reasoning_tokens_stay_inside_output() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(1_000, 0, 900, 700),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let snapshot = snapshot(&fold);

    assert_eq!(snapshot.tokens.output, 900);
    assert_eq!(snapshot.tokens.reasoning, 700);
    assert_eq!(
        snapshot.tokens.total, 1_900,
        "reasoning is part of output, not an addition to it"
    );
}

/// A scoped report must be scoped in *every* field, not only in its rows.
///
/// The failure this pins is the quiet one: a document whose money is
/// filtered to one principal but whose session count, turn count and event
/// window are still deployment-wide reads as correct, and discloses the
/// size and activity window of every other tenant to anyone holding a turn
/// key. Filtering the rows is the easy half.
#[test]
fn a_scoped_snapshot_is_scoped_in_every_field_not_only_its_rows() {
    let acme = principal("acme", "ada");
    let globex = principal("globex", "bob");

    let mut mine = LogBuilder::new("acme/ada/main");
    mine.created(Some(acme.clone())).turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(1_000, 0, 100, 0),
    );
    let mut theirs = LogBuilder::new("globex/bob/main");
    theirs
        .created(Some(globex.clone()))
        .turn(
            "r2",
            frontier("anthropic", "claude"),
            vec![],
            usage(2_000, 0, 200, 0),
        )
        .turn(
            "r3",
            frontier("anthropic", "claude"),
            vec![],
            usage(4_000, 0, 400, 0),
        );
    // A log from before the control plane: it must not be silently added to
    // anyone's row, and it must not vanish from the deployment's.
    let mut legacy = LogBuilder::new("legacy");
    legacy.turn(
        "r4",
        frontier("anthropic", "claude"),
        vec![],
        usage(8_000, 0, 800, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(mine.events());
    fold.extend(theirs.events());
    fold.extend(legacy.events());

    let config = config();
    let deployment = MetricsSnapshot::build(&fold, Scope::Deployment, &config, 9_999);
    let scoped = MetricsSnapshot::build(
        &fold,
        Scope::Principal(&PrincipalKey::from(&acme)),
        &config,
        9_999,
    );

    assert_eq!(deployment.sessions, 3);
    assert_eq!(deployment.turns, 4);
    assert_eq!(scoped.sessions, 1, "one principal, one session");
    assert_eq!(scoped.turns, 1);
    assert_eq!(scoped.calls, 1);
    assert_eq!(scoped.tokens.input, 1_000);

    // The window is the caller's own traffic, not the deployment's. Read
    // off the fixture rather than restated, so a change to the builder's
    // clock cannot make this pass by coincidence.
    let mine_first = mine.events().first().expect("the log is non-empty").at_ms;
    let mine_last = mine.events().last().expect("the log is non-empty").at_ms;
    assert_eq!(scoped.first_event_at_ms, Some(mine_first));
    assert_eq!(scoped.last_event_at_ms, Some(mine_last));
    assert!(
        deployment.last_event_at_ms > scoped.last_event_at_ms,
        "the fixture must actually distinguish the two windows"
    );

    // The scoped documents adding up to the deployment's was asserted here
    // and no longer is, deliberately. It was a real claim while two folds
    // were accumulated side by side; now the deployment's rows, turns and
    // sessions are *summed out of* the per-principal ones on the way out
    // (see `MetricsFold::view`), so the assertion reduces to `x == x`. The
    // property is now held by construction, and a test that cannot fail is
    // worse than no test: it reads as coverage.
    //
    // The half that is still a claim is above — a scoped document that
    // filtered its rows but not its window, session count or turn count.
    // That one can regress, so that one stays.
}

/// A forwarded seat is counted in tokens and priced at nothing — in the
/// same row as a keyed turn on the same model.
///
/// The mixed deployment is the sharp case, and it is why the split lives in
/// the fold rather than in a filter over rows. One BYOK project and one
/// pass-through project reaching the same hosted model produce **one** row,
/// so a reader of that row cannot tell the two apart — and pricing all of
/// it invents a bill for the half roundhouse holds no rate card for. Which
/// is exactly what the ledger has refused to do since M3.
#[test]
fn a_seat_forwarded_turn_is_counted_in_tokens_and_priced_at_nothing() {
    let mut log = LogBuilder::new("s1");
    // Billed: a key this deployment holds.
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 0, 1_000, 0),
    );
    // Accounted, not billed: the caller's own subscription seat, forwarded.
    log.seat_turn(
        "r2",
        frontier("anthropic", "claude"),
        vec![],
        usage(20_000, 0, 2_000, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let mixed = snapshot(&fold);

    assert_eq!(
        mixed.models.len(),
        1,
        "one model is one row whoever paid for it; the split has to survive \
         inside the row"
    );
    let row = &mixed.models[0];
    assert_eq!(row.tokens.total, 33_000, "every token is counted");
    assert_eq!(
        row.seat_tokens().total,
        22_000,
        "and the seat's share is visible rather than merely excluded"
    );

    // PROBE: only the keyed turn is priced. 10k uncached input at the write
    // rate plus 1k output, and nothing at all for the 22k tokens a
    // subscription paid for.
    let keyed = 10_000.0 * 3.75e-6 + 1_000.0 * 15.0e-6;
    assert!(
        (row.billed_usd() - keyed).abs() < 1e-12,
        "{} is not {keyed}",
        row.billed_usd()
    );
    assert!((mixed.savings.frontier_spend_usd - keyed).abs() < 1e-12);
    assert_eq!(
        mixed.seat_tokens.total, 22_000,
        "the headline reports the traffic it declined to price"
    );

    // CONTROL: the identical log with both turns on a key prices both, so
    // the assertion above is about the seat and not about the rate card
    // having gone missing.
    let mut both = LogBuilder::new("s2");
    both.turn(
        "r1",
        frontier("anthropic", "claude"),
        vec![],
        usage(10_000, 0, 1_000, 0),
    );
    both.turn(
        "r2",
        frontier("anthropic", "claude"),
        vec![],
        usage(20_000, 0, 2_000, 0),
    );
    let mut all_billed_fold = MetricsFold::new();
    all_billed_fold.extend(both.events());
    let all_billed = snapshot(&all_billed_fold);
    let expected = 30_000.0 * 3.75e-6 + 3_000.0 * 15.0e-6;
    assert!((all_billed.savings.frontier_spend_usd - expected).abs() < 1e-12);
    assert_eq!(all_billed.seat_tokens.total, 0);
}

/// A local turn a seat would have paid for is not a saving this deployment
/// made.
///
/// The other direction of the same rule, and the easier one to get wrong
/// because nothing about it looks like a bill: the counterfactual
/// `routing_savings_usd` reports is *money not spent*, and the hosted call a
/// pass-through session passed over would have been charged to the caller's
/// subscription. Crediting roundhouse with it is the same invented figure as
/// pricing the seat's tokens, spelled as a saving instead of a cost.
#[test]
fn routing_savings_never_credit_a_local_turn_a_seat_would_have_paid_for() {
    let mut log = LogBuilder::new("s1");
    log.seat_turn(
        "r1",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(100_000, 0, 1_000, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let seat = snapshot(&fold);

    assert_eq!(seat.tokens.total, 101_000, "the tokens are still real");
    assert_eq!(
        seat.savings.routing_savings_usd, 0.0,
        "a correlary price over a seat's traffic is a saving nobody made"
    );
    assert_eq!(
        seat.savings.routing_savings_at_decision_usd, 0.0,
        "and the router's own quote for the same road not taken says so too"
    );
    assert_eq!(seat.savings.total_usd, 0.0);

    // CONTROL: the identical turn on a project that pays with a key it
    // brought is a saving, and both estimates report it.
    let mut billed = LogBuilder::new("s2");
    billed.turn(
        "r1",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(100_000, 0, 1_000, 0),
    );
    let mut billed_fold = MetricsFold::new();
    billed_fold.extend(billed.events());
    let paid = snapshot(&billed_fold);
    let shadow = 100_000.0 * 3.75e-6 + 1_000.0 * 15.0e-6;
    assert!((paid.savings.routing_savings_usd - shadow).abs() < 1e-12);
    assert!((paid.savings.routing_savings_at_decision_usd - 0.05).abs() < 1e-12);
}

/// A local capacity price at 0.5 / 2.0 per Mtok, distinct from every
/// hosted rate so a term billed at the wrong rate cannot cancel out.
const CAPACITY: LocalCapacityPrice = LocalCapacityPrice {
    input_per_mtok_usd: 0.5,
    output_per_mtok_usd: 2.0,
};

fn priced_snapshot(fold: &MetricsFold) -> MetricsSnapshot {
    MetricsSnapshot::build(
        fold,
        Scope::Deployment,
        &config().with_local_capacity_price(CAPACITY),
        9_999,
    )
}

/// **With a price, local capacity spend is reported and the routing saving
/// is net of it**. Both savings estimates move: the
/// correlary one loses the capacity cost, and the at-decision one loses
/// the local quote the router actually served on.
#[test]
fn a_priced_local_capacity_is_reported_and_nets_the_routing_saving() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(100_000, 90_000, 1_000, 0),
    )
    .quoted(0.004);

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let priced = priced_snapshot(&fold);

    // 10k uncached at 0.5 plus 1k output at 2.0: the 90k cached tokens are free.
    let capacity = 10_000.0 * 0.5e-6 + 1_000.0 * 2.0e-6;
    let shadow = 10_000.0 * 3.75e-6 + 90_000.0 * 0.3e-6 + 1_000.0 * 15.0e-6;
    assert_eq!(priced.local_capacity_price, Some(CAPACITY));
    let reported = priced
        .savings
        .local_capacity_usd
        .expect("a priced catalog reports local capacity spend");
    assert!(
        (reported - capacity).abs() < 1e-12,
        "{reported} is not {capacity}"
    );
    let row = priced
        .models
        .iter()
        .find(|row| row.mode() == ServingMode::Local)
        .expect("the local turn has a row");
    assert!((row.capacity_usd().unwrap() - capacity).abs() < 1e-12);
    assert!(
        (row.shadow_usd() - shadow).abs() < 1e-12,
        "the row's shadow stays gross"
    );
    assert!(
        (priced.savings.routing_savings_usd - (shadow - capacity)).abs() < 1e-12,
        "the routing saving must be net of local capacity: {}",
        priced.savings.routing_savings_usd
    );
    assert!(
        (priced.savings.total_usd
            - (priced.savings.cache_savings_usd + priced.savings.routing_savings_usd))
            .abs()
            < 1e-12
    );
    assert!(
        (priced.savings.routing_savings_at_decision_usd - (0.05 - 0.004)).abs() < 1e-12,
        "the router's own saving is its hosted quote less its local quote: {}",
        priced.savings.routing_savings_at_decision_usd
    );
    let local_mode = priced
        .serving_modes
        .iter()
        .find(|mode| mode.mode == ServingMode::Local)
        .unwrap();
    assert!((local_mode.totals.capacity_usd.unwrap() - capacity).abs() < 1e-12);

    // The combined cost carries it as a labelled component, and local is
    // no longer a gap in it.
    let cost = &priced.observed_cost;
    assert!((cost.local_capacity_usd.unwrap() - capacity).abs() < 1e-12);
    assert!((cost.total_usd - (cost.serving_usd + capacity + cost.evaluation_usd)).abs() < 1e-12);
    assert_eq!(
        cost.serving_gaps.local_calls, 0,
        "priced local calls are not a gap"
    );
    assert_eq!(cost.covers, OBSERVED_COST_SCOPE_WITH_LOCAL_CAPACITY);
}

/// **Capacity spend covers a seat's local turn; the net saving subtracts
/// only the capacity of the turns whose saving it counts.** The GPU is
/// ours whoever pays for the hosted alternative, so the spend is whole; a
/// seat's turn contributes no saving, so it has nothing to be netted from.
#[test]
fn local_capacity_covers_a_seats_turn_but_nets_only_the_saving_it_offsets() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(100_000, 0, 1_000, 0),
    );
    log.seat_turn(
        "r2",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(40_000, 0, 500, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let priced = priced_snapshot(&fold);

    let billed_capacity = 100_000.0 * 0.5e-6 + 1_000.0 * 2.0e-6;
    let seat_capacity = 40_000.0 * 0.5e-6 + 500.0 * 2.0e-6;
    let shadow = 100_000.0 * 3.75e-6 + 1_000.0 * 15.0e-6;
    assert!(
        (priced.savings.local_capacity_usd.unwrap() - (billed_capacity + seat_capacity)).abs()
            < 1e-12
    );
    assert!(
        (priced.savings.routing_savings_usd - (shadow - billed_capacity)).abs() < 1e-12,
        "only the billed turn's capacity offsets the billed turn's saving: {}",
        priced.savings.routing_savings_usd
    );
}

/// **Without a price, the document marks local cost unpriced rather than
/// free**, and every figure is what it was before the price existed.
#[test]
fn without_a_local_capacity_price_local_cost_is_marked_unpriced() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        local("llama"),
        vec![candidate(frontier("anthropic", "claude"), 0.05)],
        usage(100_000, 0, 1_000, 0),
    );

    let mut fold = MetricsFold::new();
    fold.extend(log.events());
    let unpriced = snapshot(&fold);

    assert_eq!(unpriced.local_capacity_price, None);
    assert_eq!(unpriced.savings.local_capacity_usd, None);
    let shadow = 100_000.0 * 3.75e-6 + 1_000.0 * 15.0e-6;
    assert!((unpriced.savings.routing_savings_usd - shadow).abs() < 1e-12);
    assert!((unpriced.savings.routing_savings_at_decision_usd - 0.05).abs() < 1e-12);
    assert_eq!(unpriced.observed_cost.local_capacity_usd, None);
    assert_eq!(unpriced.observed_cost.serving_gaps.local_calls, 1);
    assert!(unpriced.observed_cost.incomplete);
    assert_eq!(unpriced.observed_cost.covers, OBSERVED_COST_SCOPE);

    // On the wire: an explicit `null` price and `null` figures, never a
    // zero that reads as free.
    let json = serde_json::to_value(&unpriced).unwrap();
    assert!(
        json.as_object().unwrap()["local_capacity_price"].is_null(),
        "the price is published, as `null` when unset"
    );
    assert!(
        json["savings"]["local_capacity_usd"].is_null(),
        "{}",
        json["savings"]
    );
    let row = json["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["mode"] == "local")
        .unwrap();
    assert!(row["capacity_usd"].is_null(), "{row}");
    let local_mode = json["serving_modes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mode| mode["mode"] == "local")
        .unwrap();
    assert!(
        local_mode["capacity_usd"].is_null(),
        "an unpriced aggregate must not publish a zero: {local_mode}"
    );
}

/// **`null` in a local aggregate's `capacity_usd` means unpriced and only
/// that.** A priced deployment with no local traffic yet spent `0.0` at its
/// price, the figure `savings.local_capacity_usd` already publishes; a `null`
/// there would read as the unpriced state it is not.
#[test]
fn a_priced_local_aggregate_with_no_local_calls_reads_zero_and_an_unpriced_one_null() {
    let mut log = LogBuilder::new("s1");
    log.turn(
        "r1",
        frontier("anthropic", "claude"),
        Vec::new(),
        usage(10_000, 0, 500, 0),
    );
    let mut fold = MetricsFold::new();
    fold.extend(log.events());

    let mode = |snapshot: &MetricsSnapshot, mode: ServingMode| {
        snapshot
            .serving_modes
            .iter()
            .find(|row| row.mode == mode)
            .expect("both serving modes always have a row")
            .totals
            .capacity_usd
    };

    let priced = priced_snapshot(&fold);
    assert_eq!(priced.savings.local_capacity_usd, Some(0.0));
    assert_eq!(
        mode(&priced, ServingMode::Local),
        Some(0.0),
        "priced with no local calls is a zero spend, not the unpriced null"
    );
    assert_eq!(
        mode(&priced, ServingMode::Frontier),
        None,
        "a hosted aggregate carries no capacity price"
    );

    let unpriced = snapshot(&fold);
    assert_eq!(mode(&unpriced, ServingMode::Local), None);
    assert_eq!(mode(&unpriced, ServingMode::Frontier), None);
}
