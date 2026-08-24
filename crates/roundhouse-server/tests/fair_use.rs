// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M10.1 P5: rolling fair-use windows, from the file to the `429`.
//!
//! The unit suite in `roundhouse-core::control::fair_use` proves the bucket
//! arithmetic — that a window rolls, that the narrowest is named, that a member
//! ceiling binds. What it cannot prove is the trip: that a `"fair_use"` block in
//! a control-plane file becomes a ceiling a real request is refused by, with a
//! status code and a retry time on the wire, **and that the draws which fill
//! that ceiling are recorded for a project that has no budget at all**.
//!
//! That last clause is the whole reason this file exists rather than a couple
//! more unit tests. The 2026-08-24 addendum ships budgets *unlimited* and fair
//! use *real*, so the governed project is precisely the one with no
//! `"budget"` — and `Engine::settle` returns early for exactly that project.
//! A draw hung off the settle would record nothing for every deployment this
//! mechanism was written for, and every ledger-level test would still pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::control::{
    Balance, BalanceQuery, FairUseLedger, Grant, GrantRequest, MemoryFairUseLedger, Principal,
    Settled, Settlement, SpendError, SpendLedger,
};
use roundhouse_core::ids::{SessionId, TurnId};
use roundhouse_core::item::Item;
use roundhouse_core::routing::AffinityPolicy;
use roundhouse_core::store::MemoryStore;
use roundhouse_fleet::EchoFrontierClient;
use roundhouse_server::{
    ControlPlane, ControlPlaneConfig, Conversations, EchoLocalExecutor, Engine, responses_api,
};

mod common;
use common::codex::{request, user_message};
use common::{config, frontier_catalog, key, sha256_hex};

/// A ledger that records what it was asked and grants everything.
///
/// The control that stops `a_refused_turn_took_no_grant_and_left_no_hold` being
/// tautological: the fair-use gate runs before the session is bound, so
/// "no grant was taken" is trivially true of a refused request *and* would be
/// trivially true of a gate that did nothing at all. Counting the calls on both
/// paths is what tells the two apart.
#[derive(Default)]
struct CountingLedger {
    grants: AtomicUsize,
    settles: AtomicUsize,
}

#[async_trait]
impl SpendLedger for CountingLedger {
    async fn open_grant(&self, request: GrantRequest) -> Result<Grant, SpendError> {
        self.grants.fetch_add(1, Ordering::SeqCst);
        Ok(Grant {
            granted_usd: request.requested_usd,
            state: roundhouse_core::control::LedgerState::Unconstrained,
        })
    }

    async fn settle_grant(&self, settlement: Settlement) -> Result<Settled, SpendError> {
        self.settles.fetch_add(1, Ordering::SeqCst);
        Ok(Settled {
            applied: true,
            released_usd: 0.0,
            committed_usd: settlement.actual_usd,
        })
    }

    async fn balance(&self, _query: BalanceQuery) -> Result<Balance, SpendError> {
        unreachable!("these turns never read a balance")
    }
}

/// A control plane whose one project caps tokens over five hours and declares
/// **no budget at all** — the addendum's shape, and the one an implementation
/// that hung draws off the settle would fail to govern.
fn plane(max_tokens: u64) -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [{
            "id": "bench",
            "fair_use": { "windows": [{ "window": "5h", "max_tokens": max_tokens }] },
        }],
        "users": [{ "id": "ada" }],
        "keys": [{
            "project": "bench", "user": "ada", "key_sha256": sha256_hex(&key("ada")),
        }],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "fair-use fixture")
            .expect("the fixture config must validate"),
    ))
}

fn engine(
    store: Arc<MemoryStore>,
    spend: Arc<CountingLedger>,
    fair_use: Arc<dyn FairUseLedger>,
) -> Arc<Engine<MemoryStore, ByteTokenizer>> {
    Arc::new(
        Engine::new(
            store,
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            frontier_catalog(),
            Arc::new(EchoFrontierClient::new("frontier answer")),
            Arc::new(AffinityPolicy::new()),
            config(),
        )
        .with_spend_ledger(spend)
        .with_fair_use_ledger(fair_use),
    )
}

/// The surface over the *same* store the engine writes to.
///
/// One store and not two: the compatibility surface reads a session's stored
/// items to compute the resent-prefix delta, so a router holding its own store
/// would answer `404` for every session the engine had just created — a failure
/// that looks exactly like a routing bug and is not one.
fn app(
    plane: Arc<ControlPlane>,
    engine: Arc<Engine<MemoryStore, ByteTokenizer>>,
    store: Arc<MemoryStore>,
) -> Router {
    responses_api::responses_router(plane, engine, store, Arc::new(Conversations::new()))
}

/// One `POST /v1/responses` as `bench/ada`, returning the status and the parsed
/// error object where there is one.
async fn post(app: &Router, cache_key: &str) -> (StatusCode, serde_json::Value) {
    let body = request(cache_key, vec![user_message("count some tokens")]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", key("ada")))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default();
    (status, parsed["error"].clone())
}

/// **The claim the addendum's shape depends on.**
///
/// A project with fair-use windows and no budget still fills its windows. If
/// draws were recorded from inside `Engine::settle`, this project would never
/// record one — `settle` returns before touching anything when
/// `admission.budget` is `None` — and every ledger-level test would still be
/// green.
#[tokio::test]
async fn a_project_with_fair_use_and_no_budget_still_records_its_draws() {
    let plane = plane(1);
    let fair_use = Arc::new(MemoryFairUseLedger::new());
    let spend = Arc::new(CountingLedger::default());
    let engine = engine(
        Arc::new(MemoryStore::new()),
        Arc::clone(&spend),
        Arc::clone(&fair_use) as Arc<dyn FairUseLedger>,
    );
    let admission = plane
        .membership(&Principal::new("bench", "ada"))
        .expect("the fixture declares this membership");

    // Nothing drawn yet: the window is empty, so the turn is admitted.
    assert_eq!(engine.fair_use_refusal(&admission).await.unwrap(), None);

    let session_id = SessionId::generate();
    engine.create_session(&session_id).await.unwrap();
    engine
        .run_turn(
            &session_id,
            TurnId::new("t1"),
            vec![Item::user_text("count some tokens")],
            &admission,
        )
        .await
        .expect("the first turn is served");

    // The draw landed. This is the assertion that goes red on a settle-coupled
    // implementation, and it goes red for the right reason: the project has no
    // budget, so nothing about the spend ledger was ever consulted.
    assert!(
        engine.fair_use_refusal(&admission).await.unwrap().is_some(),
        "the turn's booked usage must have filled the window; a draw recorded \
         inside `settle` would never run for a project with no budget"
    );
    assert_eq!(
        spend.grants.load(Ordering::SeqCst),
        0,
        "and the spend ledger was never touched, which is what makes the \
         assertion above about the fair-use path"
    );
    assert_eq!(spend.settles.load(Ordering::SeqCst), 0);
}

/// **A refused turn takes no grant and leaves no hold.**
///
/// The gate runs before the session is bound, so the refusal is cheap by
/// construction — and would be equally "cheap" if the gate did nothing. The
/// control below is what makes the claim mean something: the identical request
/// under a window with room opens exactly one grant.
#[tokio::test]
async fn a_refused_turn_took_no_grant_and_left_no_hold() {
    // A budgeted project this time, so there is a grant to *not* take. The
    // budget is enormous and never binds; what refuses is the window.
    let json = serde_json::json!({
        "projects": [{
            "id": "bench",
            "budget": {
                "limit_usd": 1000000.0, "window": "total", "on_exhaustion": "refuse",
            },
            "fair_use": { "windows": [{ "window": "5h", "max_tokens": 1 }] },
        }],
        "users": [{ "id": "ada" }],
        "keys": [{
            "project": "bench", "user": "ada", "key_sha256": sha256_hex(&key("ada")),
        }],
    })
    .to_string();
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "fair-use fixture").unwrap(),
    ));

    let fair_use = Arc::new(MemoryFairUseLedger::new());
    let spend = Arc::new(CountingLedger::default());
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        Arc::clone(&store),
        Arc::clone(&spend),
        Arc::clone(&fair_use) as Arc<dyn FairUseLedger>,
    );
    let app = app(Arc::clone(&plane), Arc::clone(&engine), store);

    // CONTROL, first, so the counters below cannot be explained by a transport
    // that never reached the engine at all: an empty window serves the turn and
    // opens exactly one grant.
    let (status, _) = post(&app, "sess-one").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spend.grants.load(Ordering::SeqCst), 1);

    // PROBE: the window is now over its one-token cap.
    let (status, error) = post(&app, "sess-two").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["code"], "fair_use_exceeded");

    // Give a wrongly-spawned background task a chance to run before the
    // counter below is read. `post()`'s `oneshot` call returns as soon as the
    // handler's own future resolves; if the gate ran *after* `tokio::spawn`
    // (refusing the request but leaving the spawned turn scheduled), that
    // task would sit in this current-thread runtime's queue, unpolled, until
    // something else yields. A bare read of `spend.grants` right after `post`
    // would pass whether or not that task ever ran -- it proves nothing about
    // ordering, only that the *handler's own* future didn't take a grant.
    // Yielding here hands control back to the executor, which drains any
    // already-scheduled task (the spawned turn is fully in-memory -- an
    // `EchoLocalExecutor` and an in-process store -- so it needs no real I/O
    // to run to completion once polled).
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        spend.grants.load(Ordering::SeqCst),
        1,
        "a refused turn must open no grant -- a hold taken here would sit \
         against the project's budget for a whole grant TTL for a turn that \
         never ran. This assertion is only meaningful together with the \
         `yield_now` loop above: without it, a gate moved to run *after* \
         `tokio::spawn` in `create_response` would still pass, because the \
         wrongly-spawned task would never be polled before this current-thread \
         test runtime is torn down at the end of the function -- see M10.1 \
         refute finding A."
    );
}

/// The refusal is machine-readable: a window, a scope, and a time.
///
/// An agent told only "429" can do nothing but poll, and a spent 7-day window
/// polled by an agent loop is the failure this whole mechanism exists to make
/// rare. Every field an agent would back off on is asserted by name.
#[tokio::test]
async fn the_refusal_names_the_window_and_the_earliest_retry_time() {
    let plane = plane(1);
    let fair_use = Arc::new(MemoryFairUseLedger::new());
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        Arc::clone(&store),
        Arc::new(CountingLedger::default()),
        Arc::clone(&fair_use) as Arc<dyn FairUseLedger>,
    );
    let app = app(plane, engine, store);

    let (status, _) = post(&app, "sess-one").await;
    assert_eq!(status, StatusCode::OK);

    let before = roundhouse_core::now_ms();
    let (status, error) = post(&app, "sess-two").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["code"], "fair_use_exceeded");
    assert_eq!(error["window"], "5h");
    assert_eq!(error["scope"], "project");
    assert_eq!(error["quantity"], "tokens");
    let retry_at_ms = error["retry_at_ms"].as_u64().expect("a retry time");
    assert!(
        retry_at_ms > before,
        "a retry time in the past would tell a client to try again immediately, \
         which is a poll wearing a backoff's clothes"
    );
    assert!(
        retry_at_ms <= before + 6 * 60 * 60_000,
        "and it must be inside the window it names, or it is describing some \
         other limit: {retry_at_ms} vs {before}"
    );
    // The message is prose for a human and the fields are for the client; both
    // name the same window, so a log line and a backoff cannot disagree.
    assert!(error["message"].as_str().unwrap().contains("5h"), "{error}");
}

/// The shipped posture: a project that writes no `fair_use` block is never
/// refused, whatever it draws.
///
/// The control for every assertion above — a gate that refused unconditionally
/// would satisfy all of them — and the promise every deployment written before
/// M10.1 is owed.
#[tokio::test]
async fn a_project_with_no_windows_is_never_refused_however_much_it_draws() {
    let json = serde_json::json!({
        "projects": [{ "id": "bench" }],
        "users": [{ "id": "ada" }],
        "keys": [{
            "project": "bench", "user": "ada", "key_sha256": sha256_hex(&key("ada")),
        }],
    })
    .to_string();
    let plane = Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "no-fair-use fixture").unwrap(),
    ));
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        Arc::clone(&store),
        Arc::new(CountingLedger::default()),
        Arc::new(MemoryFairUseLedger::new()),
    );
    let app = app(plane, engine, store);

    for turn in 0..4 {
        let (status, _) = post(&app, &format!("sess-{turn}")).await;
        assert_eq!(status, StatusCode::OK, "turn {turn} must be served");
    }
}

/// A rolling window is not a budget, and the reconciliation view must go on
/// saying so.
///
/// `admin_api::reconciliation` reports `unenforced` for a membership that has a
/// key and no budget — "somebody is spending and nothing is counting it" — and
/// that sentence stays true of a project whose only ceiling is a fair-use
/// window: a window counts tokens over five hours, not committed dollars, so it
/// gives the dashboard no position to read. The basis is computed from
/// `Admission::budget`, so this asserts at the input rather than through the
/// admin rig: the hazard is somebody later making a `fair_use` block resolve a
/// budget, and that is the line where it would happen.
#[tokio::test]
async fn a_fair_use_window_is_not_a_budget_and_leaves_the_view_unenforced() {
    let admission = plane(1_000)
        .membership(&Principal::new("bench", "ada"))
        .expect("the fixture declares this membership");

    assert!(
        admission.budget.is_none(),
        "a project whose only ceiling is a rolling window has no budget, so the \
         reconciliation view keeps reporting `unenforced` -- a `Some` here would \
         make the dashboard claim a committed position nothing is counting"
    );
    // CONTROL: and the window really is in force, so the assertion above is
    // about the two axes being separate rather than about the fixture having
    // configured nothing.
    assert!(!admission.fair_use.is_empty());
}

/// A plane whose *member* carries the tight window and whose project carries a
/// generous one.
///
/// The shape the ladder's own rule is about: `ada` is refused by her own
/// ceiling while `bob`, in the same project under the same generous project
/// window, is served.
fn member_capped_plane() -> Arc<ControlPlane> {
    let json = serde_json::json!({
        "projects": [{
            "id": "bench",
            "fair_use": { "windows": [{ "window": "5h", "max_tokens": 1000000 }] },
        }],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            {
                "project": "bench", "user": "ada", "key_sha256": sha256_hex(&key("ada")),
                "fair_use": { "windows": [{ "window": "5h", "max_tokens": 1 }] },
            },
            {
                "project": "bench", "user": "bob", "key_sha256": sha256_hex(&key("bob")),
            },
        ],
    })
    .to_string();
    Arc::new(ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "member fair-use fixture")
            .expect("a key may carry its own fair_use block"),
    ))
}

/// **The member tier is wired, not just modelled.**
///
/// `the_member_window_binds_even_when_the_projects_has_room` is proved at the
/// ledger over a hand-built `FairUseTerms`. This is the half a hand-built value
/// cannot reach: a `"fair_use"` block on a *key* has to survive `to_limits`,
/// `fair_use_terms` and the compiled admission table and arrive in
/// `Admission::fair_use.member` — and if it were dropped, or copied into
/// `project`, every ledger-level assertion would stay green.
#[tokio::test]
async fn a_keys_own_fair_use_block_lands_on_the_member_ceiling_and_not_the_projects() {
    let plane = member_capped_plane();

    let ada = plane
        .membership(&Principal::new("bench", "ada"))
        .expect("the fixture declares ada");
    assert_eq!(
        ada.fair_use.member.len(),
        1,
        "a key's own `fair_use` must resolve to the member list"
    );
    assert_eq!(ada.fair_use.member[0].max_tokens, Some(1));
    assert_eq!(
        ada.fair_use.project[0].max_tokens,
        Some(1_000_000),
        "and the project's window must be untouched by it -- a member block \
         copied into the project list would cap every other member too"
    );

    // CONTROL: the sibling membership, same project, no block of its own. Its
    // member list is empty and its project list is the same one — which is what
    // makes the assertion above about the key's block rather than about the
    // project's being read twice.
    let bob = plane
        .membership(&Principal::new("bench", "bob"))
        .expect("the fixture declares bob");
    assert!(
        bob.fair_use.member.is_empty(),
        "an absent member block is no member ceiling, never the project's again"
    );
    assert_eq!(bob.fair_use.project[0].max_tokens, Some(1_000_000));
}

/// And the same fact through the wire: the `429` names the member scope.
///
/// The project has a million-token window and cannot be what refused this, so a
/// refusal here is the member ceiling doing exactly what the ladder promises —
/// binding while the project has room.
#[tokio::test]
async fn a_member_ceiling_refuses_over_the_wire_while_the_project_has_room() {
    let plane = member_capped_plane();
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        Arc::clone(&store),
        Arc::new(CountingLedger::default()),
        Arc::new(MemoryFairUseLedger::new()),
    );
    let app = app(plane, engine, store);

    let (status, _) = post(&app, "sess-one").await;
    assert_eq!(status, StatusCode::OK);

    let (status, error) = post(&app, "sess-two").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        error["scope"], "member",
        "the project's window is a million tokens, so nothing about it could \
         have refused this turn: {error}"
    );
    assert_eq!(error["window"], "5h");

    // CONTROL: bob, same project, same instant, no member block. He is served —
    // which is what makes ada's refusal about her own ceiling rather than about
    // the project's counters, which her turn also filled.
    let body = request("sess-bob", vec![user_message("count some tokens")]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", key("bob")))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a member with no ceiling of his own must not inherit hers"
    );
}

/// G10 (thermo-nuclear review, M10): a 429 the pinned codex binary can act on.
///
/// codex's own `429` handling (`codex-api::api_bridge::map_api_error`, pin
/// `6344a655`, identical at the box binary's `e363b08`) recognizes exactly one
/// machine-readable shape for a rate limit it should *not* just fail on: a
/// body that deserializes as `UsageErrorResponse { error: { type, plan_type,
/// resets_at } }` with `type == "usage_limit_reached"`. Anything else on
/// `429` -- including a body with our `code`/`scope`/`window`/`retry_at_ms`
/// fields and no `type` at all -- is classified `CodexErr::RetryLimit`
/// ("exceeded retry limit, last status: 429"), discarding window/retry_at_ms.
///
/// PARTIALLY VALID, corrected mechanism: the finding describes this as codex
/// "burning its retry budget in seconds of exponential backoff" before giving
/// up. It does not. Every `RetryConfig`/`ApiRetryConfig` construction site at
/// both `6344a655` and `e363b08` -- there are ~20, `grep -rn "retry_429"
/// --include=*.rs` -- hardcodes `retry_429: false`; codex never retries a
/// `429` at the transport level regardless of provider config, so
/// `codex-client::retry::backoff` never runs for this status and no attempt
/// budget is burned. `map_api_error`'s `TransportError::Http{status: 429,
/// ..}` arm builds `CodexErr::RetryLimit` directly, on the first and only
/// attempt, the instant the body fails to match `usage_limit_reached` --
/// reusing the same terminal message and losing the same information as a
/// real retry-exhaustion would, but with no backoff delay and no wasted
/// attempts in between. The reported defect (window/retry_at_ms invisible to
/// codex, reported as an undifferentiated retry-limit error) is real; "burns
/// its retry budget" is not what happens.
///
/// This is the hermetic mirror the finding proposed: no real codex binary, no
/// network -- asserts the *fixed* behavior (a 429 codex would classify as
/// `UsageLimitReached`) and fails today because that is not what we send.
/// Ignored per CLAUDE.md: ruled, not fixed here -- see the `#[ignore]` reason.
#[tokio::test]
#[ignore = "G10 (partially valid: real defect, wrong stated mechanism -- see \
            doc comment): our 429 sends `error.code`, not `error.type`, so \
            codex's `map_api_error` (pin 6344a655 == e363b08, \
            codex-api/src/api_bridge.rs) never matches \
            `Some(\"usage_limit_reached\")` and classifies it \
            `CodexErr::RetryLimit` on the first attempt -- NOT, as the \
            finding states, after exhausting a backoff budget: `retry_429` is \
            hardcoded `false` at every RetryConfig construction site in the \
            pinned tree, so codex never retries a 429 at all. Either way \
            window/retry_at_ms are discarded. Fixing this test (send an \
            `error.type: \"usage_limit_reached\"` + `resets_at` (unix \
            seconds) alongside our existing fields) is production code, out \
            of scope for a refute stage."]
async fn the_fair_use_429_body_decodes_as_the_shape_codex_treats_specially() {
    /// Mirrors `codex-api::api_bridge::UsageErrorResponse` /
    /// `UsageErrorBody` at pin `6344a655a5966f92e009a74928fb0559b41f9093`
    /// (`codex-rs/codex-api/src/api_bridge.rs`) field-for-field: the exact
    /// struct codex's own `serde_json::from_str` targets on a `429`.
    #[derive(serde::Deserialize)]
    struct UsageErrorResponse {
        error: UsageErrorBody,
    }
    #[derive(serde::Deserialize)]
    struct UsageErrorBody {
        #[serde(rename = "type")]
        error_type: Option<String>,
        #[allow(dead_code)]
        resets_at: Option<i64>,
    }

    let plane = plane(1);
    let fair_use = Arc::new(MemoryFairUseLedger::new());
    let store = Arc::new(MemoryStore::new());
    let engine = engine(
        Arc::clone(&store),
        Arc::new(CountingLedger::default()),
        Arc::clone(&fair_use) as Arc<dyn FairUseLedger>,
    );
    let app = app(plane, engine, store);

    let (status, _) = post(&app, "sess-one").await;
    assert_eq!(status, StatusCode::OK);

    let body = request("sess-two", vec![user_message("count some tokens")]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", key("ada")))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // No `Retry-After` either, though codex's own retry loop never reads one
    // (`codex-client/src/retry.rs` backoff takes no header input at all) --
    // that half of the finding is about a generic HTTP client, not codex.

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();

    // THE CLAIM, as the fixed behavior: for codex to classify this refusal as
    // `UsageLimitReached` (scheduled, worth reporting -- not just retried into
    // exhaustion) rather than fall through to `CodexErr::RetryLimit`,
    // `map_api_error`'s `err.error.error_type.as_deref() ==
    // Some("usage_limit_reached")` must be true. `from_str` succeeds today
    // (our body is well-formed JSON with an `error` object) but `error_type`
    // decodes to `None`, because we send `error.code`, not `error.type` -- so
    // this assertion fails now, for exactly the reason G10 gives.
    let decoded: UsageErrorResponse = serde_json::from_str(&text)
        .expect("well-formed JSON with an `error` object -- codex's from_str does parse this");
    assert_eq!(
        decoded.error.error_type.as_deref(),
        Some("usage_limit_reached"),
        "G10: our 429 body has no `error.type` field (we send `error.code` \
         instead), so codex's own check in `map_api_error` is false and this \
         429 is routed to `CodexErr::RetryLimit` instead of \
         `CodexErr::UsageLimitReached` -- window/retry_at_ms are invisible to \
         codex's own error handling: {text}"
    );
}
