// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`prefix_admission`](super)'s unit tests, in their own file for the reason
//! the crate's other large modules (`mcp_api`, `claude_launch`,
//! `relay_handoff`, `control_config::directory`, `control_config::config`)
//! already are: the search this file exercises earned an inline test suite
//! wider than the search itself, and a module that keeps growing to hold it
//! is not where the next reader looks first for the search's own logic
//! (M14.0 second fix pass, P2).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use serde_json::Value;

use roundhouse_core::context::ByteTokenizer;
use roundhouse_core::event::SessionEvent;
use roundhouse_core::ids::TurnId;
use roundhouse_core::item::{ItemContent, Role};
use roundhouse_core::routing::{AffinityPolicy, CacheModel, ProviderPricing};
use roundhouse_core::store::{Lease, MemoryStore, StoreError};
use roundhouse_fleet::{
    EchoFrontierClient, FrontierModelSpec, StaticFrontierCatalog, WireProtocol,
};

use crate::engine::{EchoLocalExecutor, EngineConfig};

use super::*;

fn user(text: &str) -> Item {
    Item::user_text(text)
}

fn assistant(text: &str) -> Item {
    Item::assistant_text(text, ResponseId::new("resp_1"))
}

#[test]
fn a_grown_history_yields_only_what_the_session_lacks() {
    let stored = vec![user("hello"), assistant("hi")];
    let claimed = vec![
        user("hello"),
        // The client's copy carries no response stamp; ours does.
        Item {
            role: Role::Assistant,
            content: ItemContent::Text { text: "hi".into() },
            response_id: None,
        },
        user("again"),
    ];
    assert_eq!(
        suffix_after(&stored, &claimed),
        Some(vec![user("again")]),
        "a stamped assistant item must still match the client's copy of it"
    );
}

#[test]
fn a_retry_of_an_answered_turn_yields_nothing_to_append() {
    let stored = vec![user("hello"), assistant("hi")];
    // The retry predates the answer, because the client never saw it.
    assert_eq!(suffix_after(&stored, &[user("hello")]), Some(Vec::new()));
}

#[test]
fn an_edited_history_is_refused_rather_than_appended() {
    let stored = vec![user("hello"), assistant("hi")];
    assert_eq!(suffix_after(&stored, &[user("goodbye")]), None);
}

fn configuration(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

fn stored(items: Vec<Item>) -> StoredConversation {
    let configuration_len = turn_configuration_len(&items);
    StoredConversation {
        items,
        configuration_len,
    }
}

/// **A rewritten configuration run is recorded, not forked on** (F7), and
/// the conversation underneath it is still admitted strictly.
///
/// The four cases are the whole ruling. Note what the delta contains in the
/// second: the *new* run and only the genuinely new history — the run is
/// re-recorded because it changed, and the projection puts it at the head.
#[test]
fn a_changed_configuration_run_is_admitted_and_a_changed_history_is_not() {
    let session = stored(vec![configuration("v1"), user("hello"), assistant("hi")]);
    let history = [
        user("hello"),
        Item {
            role: Role::Assistant,
            content: ItemContent::Text { text: "hi".into() },
            response_id: None,
        },
        user("again"),
    ];

    // Unchanged: nothing about the configuration is re-recorded.
    let mut claimed = vec![configuration("v1")];
    claimed.extend_from_slice(&history);
    assert_eq!(admit(&session, &claimed), Some(vec![user("again")]));

    // Rewritten: the new run leads the delta, ahead of the new history.
    let mut claimed = vec![configuration("v2")];
    claimed.extend_from_slice(&history);
    assert_eq!(
        admit(&session, &claimed),
        Some(vec![configuration("v2"), user("again")]),
    );

    // A run that gained a block is a changed run, not a matching prefix.
    let mut claimed = vec![configuration("v1"), configuration("extra")];
    claimed.extend_from_slice(&history);
    assert_eq!(
        admit(&session, &claimed),
        Some(vec![
            configuration("v1"),
            configuration("extra"),
            user("again")
        ]),
    );

    // And the history is still strict: rewriting *it* forks, whatever the
    // configuration says. This is the assertion that keeps the tolerance
    // narrow.
    assert_eq!(
        admit(&session, &[configuration("v1"), user("goodbye")]),
        None
    );
    assert_eq!(
        admit(&session, &[configuration("v2"), user("goodbye")]),
        None
    );
}

/// A claim with no configuration of its own says nothing about the
/// session's, rather than claiming it is now empty.
///
/// An empty run has no items to append and so nothing to record; forking
/// over it would punish exactly the bare `curl` the anonymous arm exists to
/// serve.
#[test]
fn a_claim_carrying_no_configuration_leaves_the_stored_run_alone() {
    let session = stored(vec![configuration("v1"), user("hello"), assistant("hi")]);
    assert_eq!(
        admit(
            &session,
            &[
                user("hello"),
                Item {
                    role: Role::Assistant,
                    content: ItemContent::Text { text: "hi".into() },
                    response_id: None,
                },
                user("again"),
            ]
        ),
        Some(vec![user("again")]),
    );
}

// -----------------------------------------------------------------------
// The search, at `bind_prefix`'s own level (R13, M14.0)
// -----------------------------------------------------------------------

fn ada() -> Principal {
    Principal::new("acme", "ada")
}

/// One priced frontier model, so `Engine::create_session`'s policy lookup
/// has somewhere to resolve. `bind_prefix` never dispatches a turn, so
/// nothing about routing or pricing is exercised below.
fn catalog() -> StaticFrontierCatalog {
    StaticFrontierCatalog::new(vec![FrontierModelSpec {
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
    }])
}

/// One node: a store, an engine over it, this node's generation counter,
/// and the one cache key every claim in a test is made against.
///
/// **Every test below drives the search through this, and that is the
/// point** (M14.0 review, F5). The six tests this rung first shipped each
/// repeated the same five-line setup and the same seven-argument call, so
/// a change to `bind_prefix`'s signature or to what a node needs to answer
/// a claim was six edits, and the *interesting* line of each test — the
/// claim, and where it must land — was buried in the fixture around it.
///
/// Generic over the store because the same properties are asserted against
/// three of them: `MemoryStore` for the hermetic majority, a real
/// `RedisSessionStore` for the two that are about a genuine restart, and a
/// counting double for the one that is about how many reads a claim costs.
struct Rig<S: SessionStore> {
    store: Arc<S>,
    engine: Engine<S, ByteTokenizer>,
    conversations: Conversations,
    principal: Principal,
    key: String,
}

impl Rig<MemoryStore> {
    fn new(key: &str) -> Self {
        Self::over(Arc::new(MemoryStore::new()), key)
    }
}

impl<S: SessionStore> Rig<S> {
    fn over(store: Arc<S>, key: &str) -> Self {
        let engine = Engine::new(
            Arc::clone(&store),
            ByteTokenizer,
            Arc::new(EchoLocalExecutor::new("local answer")),
            catalog(),
            Arc::new(EchoFrontierClient::new("answer")),
            Arc::new(AffinityPolicy::new()),
            EngineConfig::default(),
        );
        Self {
            store,
            engine,
            conversations: Conversations::new(),
            principal: ada(),
            key: key.to_string(),
        }
    }

    /// Another node's view of the same store and the same key: its own
    /// generation counter, starting from nothing, and nothing else shared.
    ///
    /// This is what both a restart and a second node look like from the
    /// store's side, and the difference between them is not observable
    /// here — which is exactly the claim R13 makes.
    fn other_node(&self) -> Self {
        Self::over(Arc::clone(&self.store), &self.key)
    }

    fn generation(&self, generation: u32) -> SessionId {
        bound_session(&self.key, generation)
    }

    /// Writes `items` straight to a generation's log, bypassing the engine.
    ///
    /// This is what "the store already holds a log under this generation"
    /// means for the tests below: the log was written by an engine that, in
    /// the restart scenario, belongs to a process that no longer exists.
    /// Going through a real turn would only prove the property for logs
    /// *this* process wrote, which is exactly the case R13 is not about.
    async fn seed(&self, generation: u32, items: Vec<Item>) {
        let session_id = self.generation(generation);
        self.store
            .create_session(&session_id, "test-policy")
            .await
            .expect("seed session creation");
        let lease = self
            .store
            .acquire_lease(&session_id, "seed-node", 60_000)
            .await
            .expect("seed lease request")
            .expect("seed lease granted");
        let kinds = items
            .into_iter()
            .map(|item| SessionEventKind::ItemAppended { item })
            .collect();
        self.store
            .append_events(&lease, kinds)
            .await
            .expect("seed append");
    }

    async fn bind(&self, claimed: Vec<Item>) -> Result<(SessionId, Vec<Item>), ApiError> {
        bind_prefix(
            &self.engine,
            self.store.as_ref(),
            &self.conversations,
            &ControlPlane::Open,
            &self.principal,
            &self.key,
            claimed,
        )
        .await
    }
}

/// **(a) restart-then-fork.** A store pre-seeded exactly as it would be
/// after a real restart — generation zero holding the pre-restart log,
/// `#g1` holding what the pre-restart process had already forked to — and
/// a node whose counter re-derives generation zero from nothing. A claim
/// that disagrees with generation zero and equals `#g1` (plus one
/// genuinely new turn) must land on `#g1` with only that turn appended:
/// the D1 inventory's §6(a), verified.
#[tokio::test]
async fn a_restart_lands_on_the_generation_the_store_already_holds() {
    let rig = Rig::new("acme/ada/restart");
    rig.seed(0, vec![user("hello")]).await;
    let g1 = vec![user("hello, redone"), assistant("hi again")];
    rig.seed(1, g1.clone()).await;

    let mut claimed = g1;
    claimed.push(user("more"));
    let (session_id, delta) = rig
        .bind(claimed)
        .await
        .expect("the claim agrees with #g1 once it is actually checked");

    assert_eq!(
        session_id,
        rig.generation(1),
        "the turn must land on the generation the store already holds, \
         not past a log this process forgot"
    );
    assert_eq!(
        delta,
        vec![user("more")],
        "and only the genuinely new turn may be appended — appending the \
         #g1 history a second time on top of itself is the duplicated \
         prefix R13 exists to prevent"
    );
}

/// **(b) the claim disagrees with every generation the store holds.** Two
/// generations are occupied and disagree; the turn must land on the first
/// generation the store has never heard of and take the claim whole there,
/// exactly as an ordinary first divergence does, just one generation
/// further out.
#[tokio::test]
async fn a_claim_disagreeing_with_every_existing_generation_opens_a_fresh_one() {
    let rig = Rig::new("acme/ada/exhausted-generations");
    rig.seed(0, vec![user("hello")]).await;
    rig.seed(1, vec![user("hello, redone")]).await;

    let claimed = vec![user("a completely different opening")];
    let (session_id, delta) = rig
        .bind(claimed.clone())
        .await
        .expect("two disagreements is well inside the bound");

    assert_eq!(
        session_id,
        rig.generation(2),
        "generation one is occupied and disagrees too, so the turn must \
         land on the first generation the store has never seen"
    );
    assert_eq!(
        delta, claimed,
        "a session the store never held takes the claim whole"
    );
}

/// **(c) the bound.** Every generation the search can reach disagrees, so
/// it never finds a home and must refuse loudly — naming the cache key and
/// the tally — rather than searching forever or guessing. Nothing is
/// appended anywhere: every generation's log is exactly the one item
/// [`Rig::seed`] wrote it.
///
/// **The tally is a count, not the constant** (M14.0 review, F8). Nine
/// generations disagree here — the node's current one, plus the eight the
/// upward walk is allowed — and nine is what the refusal must report. The
/// constant cannot stand in for it: the search walks in two directions and
/// stops early on the first free slot, so the number of generations one
/// request actually read back is not derivable from the bound.
#[tokio::test]
async fn a_claim_disagreeing_past_the_bound_is_refused_with_what_it_probed() {
    let rig = Rig::new("acme/ada/looping");
    for generation in 0..=MAX_PREFIX_PROBES {
        rig.seed(generation, vec![user(&format!("generation {generation}"))])
            .await;
    }

    let error = rig
        .bind(vec![user("none of the above")])
        .await
        .expect_err("every generation the search can reach disagrees");

    assert_eq!(error.status(), StatusCode::CONFLICT);
    assert_eq!(error.code(), "prefix_admission_exhausted");
    let detail = error
        .detail()
        .expect("a client acting on this refusal needs the key and the count, not English");
    assert_eq!(
        detail.get("cache_key").and_then(Value::as_str),
        Some(rig.key.as_str())
    );
    assert_eq!(
        detail.get("attempts").and_then(Value::as_u64),
        Some(u64::from(MAX_PREFIX_PROBES) + 1),
        "F8: the refusal reports the generations this request actually \
         read back and found disagreeing — the current one plus every \
         step of the upward walk — and not the constant that bounded it"
    );

    for generation in 0..=MAX_PREFIX_PROBES {
        let stored = stored_conversation(rig.store.as_ref(), &rig.generation(generation))
            .await
            .expect("every pre-seeded generation must still read back");
        assert_eq!(
            stored.items.len(),
            1,
            "generation {generation} must be exactly what `seed` wrote — \
             a refused request must append nothing anywhere"
        );
    }
}

/// **F9 (M14.0 review): a refused request moves nothing.**
///
/// `latest`'s own contract is "the last session this principal drove a
/// turn on", and a wholly refused request drove none. The predecessor of
/// this search forked per attempt, so a refusal left `latest` — and, on
/// the same counter, what [`Conversations::resolve`] answers — naming the
/// final *disagreeing* generation it happened to be probing: a session
/// this node never created, whose log disagreed with the client, and that
/// a later MCP call with no explicit conversation would have been answered
/// with.
///
/// The baseline is established with a real [`Conversations::commit`],
/// standing in for the last turn this node actually served on the key, so
/// the assertion is about the refused request's own writes and not about
/// whether the table was empty to begin with.
#[tokio::test]
async fn a_refused_request_leaves_latest_and_the_binding_where_the_last_turn_left_them() {
    let rig = Rig::new("acme/ada/looping-latest");
    for generation in 0..=MAX_PREFIX_PROBES {
        rig.seed(generation, vec![user(&format!("generation {generation}"))])
            .await;
    }

    let qualified_key = ControlPlane::Open.qualify(&rig.principal, &rig.key);
    rig.conversations.commit(&rig.principal, &qualified_key, 0);
    let latest_before = rig.conversations.latest(&rig.principal);
    let resolved_before = rig.conversations.resolve(&qualified_key);

    let error = rig
        .bind(vec![user("none of the above")])
        .await
        .expect_err("every generation the search can reach disagrees");
    assert_eq!(error.code(), "prefix_admission_exhausted");

    assert_eq!(
        rig.conversations.latest(&rig.principal),
        latest_before,
        "F9: a wholly refused request served no turn, so `latest` must be \
         exactly where the last one left it — not moved on to a dead \
         generation the search merely looked at"
    );
    assert_eq!(
        rig.conversations.resolve(&qualified_key),
        resolved_before,
        "F9: `resolve` reads the same counter, so it must not be left \
         naming a generation this node never served either"
    );
}

/// **F6 (M14.0 review): a verbatim retry is refused identically.**
///
/// The client's claim has not changed, so whatever made every generation
/// the first attempt probed disagree is still true of every one of them.
/// That is the property the bound is *for*: refusing a client stuck
/// disagreeing with the log, not merely spending its first eight attempts
/// before doing what it was trying to do anyway. Claude Code retries a 409
/// unconditionally, so a bound that only stopped one request would be
/// invisible to the user and would admit the second attempt whole.
///
/// One [`Rig`] and therefore one counter, standing in for one node serving
/// both the original request and the retry.
#[tokio::test]
async fn a_verbatim_retry_of_a_refused_claim_is_refused_identically() {
    let rig = Rig::new("acme/ada/looping-retry");
    for generation in 0..=MAX_PREFIX_PROBES {
        rig.seed(generation, vec![user(&format!("generation {generation}"))])
            .await;
    }
    let claimed = vec![user("none of the above")];

    let first = rig
        .bind(claimed.clone())
        .await
        .expect_err("every seeded generation disagrees — the first request must refuse");
    let second = rig.bind(claimed).await.expect_err(
        "F6: the retry disagrees with exactly the generations the first \
         attempt did — if this is Ok, the refusal moved the counter and \
         the retry resumed past the bound onto a free generation instead",
    );

    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(second.code(), "prefix_admission_exhausted");
    assert_eq!(
        second.detail().and_then(|d| d.get("attempts").cloned()),
        first.detail().and_then(|d| d.get("attempts").cloned()),
        "F6: identically refused means the same generations were probed, \
         not merely that some refusal came back"
    );
    assert!(
        no_such_generation(rig.store.as_ref(), &rig.generation(MAX_PREFIX_PROBES + 1)).await,
        "F6: neither attempt may leave a generation behind past the ones \
         it probed — that free slot is what the retry would have been \
         admitted onto"
    );
}

/// Whether the store has never heard of this generation.
///
/// The honest spelling of "nothing was created here": an absent session's
/// `last_seq` is an error, where an existing but empty one answers zero.
async fn no_such_generation<S: SessionStore>(store: &S, session_id: &SessionId) -> bool {
    store.last_seq(session_id).await.is_err()
}

/// **(d) control: agreement never moves.** The ordinary continuation case,
/// asserted at `bind_prefix`'s own level rather than only through
/// [`admit`], so a change to the search cannot silently start diverging
/// the common case it must leave alone.
#[tokio::test]
async fn an_agreeing_claim_continues_the_current_generation() {
    let rig = Rig::new("acme/ada/agrees");
    rig.seed(0, vec![user("hello")]).await;

    let (session_id, delta) = rig
        .bind(vec![user("hello"), user("again")])
        .await
        .expect("a prefix match is not a disagreement");

    assert_eq!(session_id, rig.generation(0));
    assert_eq!(delta, vec![user("again")]);
}

/// **(d) control: the ordinary first divergence is unchanged.** Only
/// generation zero exists and disagrees — no restart, no generation the
/// store already holds further out — so the turn lands on a genuinely
/// fresh session and takes the claim whole, exactly as before R13.
#[tokio::test]
async fn the_ordinary_first_divergence_still_takes_the_claim_whole() {
    let rig = Rig::new("acme/ada/first-fork");
    rig.seed(0, vec![user("hello")]).await;

    let claimed = vec![user("goodbye")];
    let (session_id, delta) = rig
        .bind(claimed.clone())
        .await
        .expect("one disagreement is well inside the bound");

    assert_eq!(
        session_id,
        rig.generation(1),
        "a generation the store has never seen is the fresh case"
    );
    assert_eq!(delta, claimed);
}

/// **(e) the bound generation is actually reached, not merely counted.**
///
/// (M14.0 fix review, F1.) The refusal test above cannot tell "the search
/// made every probe it was allowed and all of them failed" from "it made
/// one fewer and would have failed anyway" — a fixture where nothing
/// agrees reads the same either way. This is the other direction: the
/// generation exactly at the bound *agrees*, so only a walk that actually
/// makes all [`MAX_PREFIX_PROBES`] probes reaches it, and an
/// off-by-one that stops one short refuses instead of admitting.
#[tokio::test]
async fn the_generation_at_the_bound_is_actually_probed() {
    let rig = Rig::new("acme/ada/bound-generation");
    for generation in 0..MAX_PREFIX_PROBES {
        rig.seed(generation, vec![user(&format!("generation {generation}"))])
            .await;
    }
    let agreeing = vec![user("the surviving generation")];
    rig.seed(MAX_PREFIX_PROBES, agreeing.clone()).await;

    let mut claimed = agreeing;
    claimed.push(user("plus one more"));
    let (session_id, delta) = rig.bind(claimed).await.expect(
        "MAX_PREFIX_PROBES disagreements are still inside the \
         bound — the walk must make every probe it is allowed",
    );

    assert_eq!(session_id, rig.generation(MAX_PREFIX_PROBES));
    assert_eq!(delta, vec![user("plus one more")]);
}

/// **F11 (M14.0 review): one claimed history has one home, whatever a
/// node's counter says.**
///
/// A search that only walked *up* from this node's counter judged a claim
/// against the generation the counter happened to name and nothing older.
/// So a node that had served a divergent turn in between saw a resume of
/// an *earlier* generation disagree, moved past it, and took the whole
/// claim onto a new generation — duplicating the prefix the earlier one
/// already held — while a node whose counter was still at zero recognized
/// the same claim as a continuation and appended only the delta. One
/// claim, two homes, differing by which node answered.
///
/// `#g1` is created by driving a genuinely disagreeing claim through the
/// serving node, so its counter advances for the real reason rather than
/// being seeded to look as if it had.
#[tokio::test]
async fn one_claimed_history_has_one_home_whatever_a_nodes_counter_says() {
    let serving = Rig::new("acme/ada/two-homes");
    serving
        .seed(0, vec![user("hello"), assistant("ANSWER")])
        .await;

    let (forked, _) = serving
        .bind(vec![user("goodbye")])
        .await
        .expect("goodbye disagrees with generation zero and opens #g1");
    assert_eq!(forked, serving.generation(1));
    serving
        .seed(1, vec![user("goodbye"), assistant("ANSWER")])
        .await;

    // The same claim — generation zero's own history plus one genuinely
    // new turn — put to a node whose counter is at one and to a node whose
    // counter is at zero.
    let resume = vec![user("hello"), assistant("ANSWER"), user("more")];
    let fresh = serving.other_node();
    let (serving_session, serving_delta) = serving
        .bind(resume.clone())
        .await
        .expect("the serving node still finds a generation to land on");
    let (fresh_session, fresh_delta) = fresh
        .bind(resume)
        .await
        .expect("the fresh node's counter starts at generation zero, which agrees");

    assert_eq!(fresh_session, fresh.generation(0));
    assert_eq!(fresh_delta, vec![user("more")]);
    assert_eq!(
        serving_session, fresh_session,
        "F11: one claimed history landed on two different generations \
         depending only on which node's counter served it"
    );
    assert_eq!(
        serving_delta, fresh_delta,
        "F11: the serving node re-appended the whole claim — including \
         [hello, ANSWER], which generation zero already holds — instead \
         of the one-item delta the fresh node computed"
    );
}

/// The longest agreeing generation wins, and this is the case where the
/// two directions of the search disagree about the answer.
///
/// Generation zero and generation one both agree with the claim — zero
/// holds its opening turn, one holds that turn *and* the next — so a
/// search that took the first agreement it walked past would continue zero
/// and append, a second time, the two turns one already holds. There is no
/// way to prove the rule with a fixture where only one generation agrees.
///
/// The node's counter is at *two*, which disagrees, so the search has to
/// walk down over both of them: this is the arbitration itself, not the
/// current-generation short circuit.
#[tokio::test]
async fn two_agreeing_generations_resolve_to_the_one_holding_more() {
    let rig = Rig::new("acme/ada/longest-agreeing");
    rig.seed(0, vec![user("hello")]).await;
    rig.seed(1, vec![user("hello"), assistant("hi"), user("again")])
        .await;
    rig.seed(2, vec![user("a different opening")]).await;
    rig.conversations.commit(
        &rig.principal,
        &ControlPlane::Open.qualify(&rig.principal, &rig.key),
        2,
    );

    let (session_id, delta) = rig
        .bind(vec![
            user("hello"),
            assistant("hi"),
            user("again"),
            user("and again"),
        ])
        .await
        .expect("two of the three generations agree with this claim");

    assert_eq!(
        session_id,
        rig.generation(1),
        "generation zero agrees too, but continuing it would re-append \
         the two turns generation one already holds"
    );
    assert_eq!(delta, vec![user("and again")]);
}

/// **F10 (M14.0 review): an empty generation another node is mid-turn on
/// is not a home.**
///
/// A log with no items agrees with every claim trivially, so an empty
/// generation another node created one instruction ago read exactly like a
/// free one: the claim was admitted onto it whole, and `run_turn` then
/// died acquiring the lease that node already held — in-stream, after
/// admission had reported success, with nothing appended and nothing moved
/// on, so a retry repeated the outcome until the other node's items landed
/// or its lease lapsed.
///
/// The setup is the collision itself: `#g0` holds committed history, so
/// the claim must move past it; `#g1` is created and leased by `node-b`
/// *before* the claim is admitted.
#[tokio::test]
async fn an_empty_generation_another_writer_holds_is_not_a_home() {
    use crate::control_config::Admission;

    let rig = Rig::new("acme/ada/lease-race");
    rig.seed(0, vec![user("hello")]).await;

    let g1 = rig.generation(1);
    rig.store
        .create_session(&g1, "test-policy")
        .await
        .expect("the other node creates #g1");
    let _node_b = rig
        .store
        .acquire_lease(&g1, "node-b", 60_000)
        .await
        .expect("the other node's lease request")
        .expect("#g1 is fresh and unleased until now");

    let claimed = vec![user("goodbye")];
    let (session_id, delta) = rig
        .bind(claimed.clone())
        .await
        .expect("a collision on one generation is not a reason to refuse the turn");

    assert_eq!(
        session_id,
        rig.generation(2),
        "F10: #g1 is empty but leased — another node's slot, not a free \
         one — so the claim must land past it"
    );
    assert_eq!(delta, claimed);
    assert!(
        rig.engine
            .run_turn(
                &session_id,
                TurnId::new("lease-race-turn"),
                delta,
                &Admission::open()
            )
            .await
            .is_ok(),
        "F10: whatever admission hands back is what the caller runs a turn \
         against, so it must not name a session this node provably cannot \
         write to"
    );
}

/// The other half of F10's rule, and the reason it is narrow: an empty
/// generation *nobody* is writing is ours.
///
/// This is the shape a request leaves behind when it opened a generation
/// and never appended to it — the client hung up, or the turn was refused
/// downstream. The next claim must land on that slot rather than opening
/// another beside it; a fix that skipped every empty generation would pass
/// F10's test and quietly mint a session per attempt here.
#[tokio::test]
async fn an_empty_generation_nobody_is_writing_is_a_home() {
    let rig = Rig::new("acme/ada/empty-and-idle");
    rig.seed(0, vec![user("hello")]).await;
    rig.store
        .create_session(&rig.generation(1), "test-policy")
        .await
        .expect("a previous request opened #g1 and appended nothing");

    let claimed = vec![user("goodbye")];
    let (session_id, delta) = rig
        .bind(claimed.clone())
        .await
        .expect("an idle empty generation has nothing to disagree with");

    assert_eq!(
        session_id,
        rig.generation(1),
        "an empty, unleased generation is this deployment's own free slot"
    );
    assert_eq!(delta, claimed);
    assert!(
        no_such_generation(rig.store.as_ref(), &rig.generation(2)).await,
        "and no second slot may be minted beside it"
    );
}

/// **P1 (M14.0 second fix pass): the upward walk's probe of a free slot
/// must not create it merely because it was read.**
///
/// This is the residual the first fix pass left: [`probe`] asked
/// existence by calling `create_session` (create-if-missing), so a walk
/// that probed past the home it eventually landed on left a write behind
/// at every fresh generation it merely looked at. Here the node's counter
/// is at generation 1, which disagrees, so the search has to walk both
/// directions: upward to `#g2`, which the store has never held (the free
/// slot), and downward to `#g0`, which agrees and holds more of the claim
/// — so the claim lands on `#g0` and `#g2` is never the home. Under the
/// old create-as-you-probe `probe`, the upward step would still have
/// minted `#g2` in the store on its way past it; the fix is that probing
/// is read-only, so a generation the search rejects is left exactly as it
/// found it — nonexistent.
#[tokio::test]
async fn the_upward_walks_free_slot_is_not_created_when_the_home_is_elsewhere() {
    let rig = Rig::new("acme/ada/no-residual-fork");
    rig.seed(0, vec![user("hello")]).await;
    rig.seed(1, vec![user("goodbye")]).await;
    rig.conversations.commit(
        &rig.principal,
        &ControlPlane::Open.qualify(&rig.principal, &rig.key),
        1,
    );

    let resume = vec![user("hello"), user("more")];
    let (session_id, delta) = rig
        .bind(resume)
        .await
        .expect("generation zero agrees with the resumed claim");

    assert_eq!(
        session_id,
        rig.generation(0),
        "the downward walk's agreement at #g0 is the home, not the free \
         slot the upward walk merely passed on its way to a Fresh answer"
    );
    assert_eq!(delta, vec![user("more")]);
    assert!(
        no_such_generation(rig.store.as_ref(), &rig.generation(2)).await,
        "P1: #g2 was only ever probed, never landed on — a probe that \
         writes nothing must leave it exactly as it found it, absent"
    );
}

/// **P1 (M14.0 second fix pass): a wholly refused request never calls
/// `create_session` at all.**
///
/// Every generation the search can reach disagrees, so the request
/// commits nothing — but the old `probe` called `create_session` on every
/// generation it merely asked about, including the ones it went on to
/// reject. `create_session` is create-if-missing, so those calls did not
/// mint new sessions here (every generation was pre-seeded and already
/// existed), yet they were real store writes attempted for no reason: a
/// probe is a question, and a question that dials the same write path the
/// commit does is not read-only merely because the answer happens not to
/// change anything.
#[tokio::test]
async fn a_refused_request_never_calls_create_session() {
    let rig = Rig::over(
        Arc::new(CountingStore::new()),
        "acme/ada/refused-writes-nothing",
    );
    for generation in 0..=MAX_PREFIX_PROBES {
        rig.seed(generation, vec![user(&format!("generation {generation}"))])
            .await;
    }
    let before = rig.store.create_session_call_count();

    let error = rig
        .bind(vec![user("none of the above")])
        .await
        .expect_err("every generation the search can reach disagrees");

    assert_eq!(error.code(), "prefix_admission_exhausted");
    assert_eq!(
        rig.store.create_session_call_count(),
        before,
        "P1: a refusal commits nothing, and `create_session` is the \
         commit step's own call — a probe that reaches for it too is \
         writing before the home is known, which R13' says never to do"
    );
}

/// A [`SessionStore`] double that delegates every call to a real
/// [`MemoryStore`] and additionally counts `read_events`, so a test can
/// assert *how many times* a claim asked to read a log rather than only
/// what it read.
struct CountingStore {
    inner: MemoryStore,
    read_events_calls: AtomicUsize,
    create_session_calls: AtomicUsize,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            read_events_calls: AtomicUsize::new(0),
            create_session_calls: AtomicUsize::new(0),
        }
    }

    fn read_events_call_count(&self) -> usize {
        self.read_events_calls.load(Ordering::SeqCst)
    }

    fn create_session_call_count(&self) -> usize {
        self.create_session_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl SessionStore for CountingStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        self.create_session_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.create_session(session_id, model_policy).await
    }

    async fn acquire_lease(
        &self,
        session_id: &SessionId,
        node_id: &str,
        ttl_ms: u64,
    ) -> Result<Option<Lease>, StoreError> {
        self.inner.acquire_lease(session_id, node_id, ttl_ms).await
    }

    async fn renew_lease(&self, lease: &Lease, ttl_ms: u64) -> Result<Option<Lease>, StoreError> {
        self.inner.renew_lease(lease, ttl_ms).await
    }

    async fn release_lease(&self, lease: &Lease) -> Result<(), StoreError> {
        self.inner.release_lease(lease).await
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn append_events(
        &self,
        lease: &Lease,
        kinds: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.inner.append_events(lease, kinds).await
    }

    async fn read_events(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.read_events_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read_events(session_id, after_seq, limit).await
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        self.inner.last_seq(session_id).await
    }
}

/// **F2 (M14.0 review): a fresh key's first claim costs no read at all.**
///
/// The store has already said this generation did not exist, which is the
/// same fact that lets a fresh slot take a claim whole further out in the
/// search. Projecting the necessarily-empty log anyway to run it through
/// [`admit`] is work with one possible answer — and it was paid on the
/// first turn of every conversation this deployment serves, because the
/// predecessor spelled the admission step twice and only the second copy
/// consumed the boolean.
#[tokio::test]
async fn a_fresh_keys_first_claim_costs_no_read_at_all() {
    let rig = Rig::over(Arc::new(CountingStore::new()), "acme/ada/fresh-key");
    let claimed = vec![user("hello")];

    let (session_id, delta) = rig
        .bind(claimed.clone())
        .await
        .expect("a fresh key's first-ever claim has nothing to disagree with");

    assert_eq!(session_id, rig.generation(0));
    assert_eq!(delta, claimed);
    assert_eq!(
        rig.store.read_events_call_count(),
        0,
        "F2: `create_session` already said this generation is fresh, so \
         there is no log to read and nothing a projection of it could say"
    );
}

/// **F7 (M14.0 review), and (a) over a real store.** The same restart
/// property as `a_restart_lands_on_the_generation_the_store_already_holds`,
/// proved where the two halves of the restart are two genuinely separate
/// connections and the persistence is real rather than simulated by
/// pre-seeding one process's own map.
///
/// It is the evidence for the cost sentence in
/// [`conversations`](crate::conversations)' module doc: an agreeing
/// restart lands on the generation the store holds, with only the new turn
/// in the delta — so it opens no session, prices nothing cold, and loses
/// no warm prefix. What it pays is the one extra read of the generation it
/// walked past.
///
/// Gated like `tests/redis_store.rs`: `#[ignore]` is the one skip the
/// harness reports, opted into with `--include-ignored`, and a missing
/// `ROUNDHOUSE_TEST_REDIS_URL` then fails loudly.
#[tokio::test]
#[ignore = "needs a real Redis: set ROUNDHOUSE_TEST_REDIS_URL and pass --include-ignored"]
async fn a_restart_lands_on_the_stores_existing_generation_over_real_redis() {
    use roundhouse_store_redis::RedisSessionStore;
    use roundhouse_store_redis::test_support::connect_from_env;

    // Unique per run so a leftover key from an earlier failed run cannot
    // make this pass — or fail — for the wrong reason.
    let key = format!("acme/ada/restart-{}", SessionId::generate());

    // The pre-restart process: writes generation zero's log, then the log
    // of the generation it had already forked to. Its connection and its
    // counter are dropped here, standing in for the process exiting.
    {
        let before: Rig<RedisSessionStore> = Rig::over(Arc::new(connect_from_env().await), &key);
        before.seed(0, vec![user("hello")]).await;
        before
            .seed(1, vec![user("hello, redone"), assistant("hi again")])
            .await;
    }

    // The post-restart process: a fresh connection to the same Redis and a
    // counter that has re-derived generation zero from nothing.
    let after: Rig<RedisSessionStore> = Rig::over(Arc::new(connect_from_env().await), &key);
    let (session_id, delta) = after
        .bind(vec![
            user("hello, redone"),
            assistant("hi again"),
            user("more"),
        ])
        .await
        .expect("the claim agrees with #g1 once it is actually checked");

    assert_eq!(
        session_id,
        after.generation(1),
        "F7: the turn lands on the generation Redis already holds for this \
         key — a fresh generation here would be the avoidable fork the \
         module doc's cost sentence used to price"
    );
    assert_eq!(
        delta,
        vec![user("more")],
        "F7: only the genuinely new turn is appended, so the #g1 prefix \
         stays warm rather than being re-sent on top of itself"
    );
}

/// **F1 (M14.0 review): the shared function keeps its own doc comment.**
///
/// [`bind_prefix`] is the function both dialects reach admission through,
/// and it is linked from [`conversations`](crate::conversations) and
/// [`messages_api`](crate::messages_api). When
/// [`MAX_PREFIX_PROBES`] was first added it was inserted with its
/// own doc comment pasted onto the *end* of `bind_prefix`'s, with no blank
/// line between the two items — so rustdoc read the whole run as one
/// comment, attached it to the constant, and rendered the most-shared
/// function in this crate with no documentation at all.
///
/// This walks the source text rather than a parsed AST: no `syn`-family
/// crate is a workspace dependency, and pulling one in only to check
/// comment placement would be a heavier fix than the defect.
#[test]
fn bind_prefix_keeps_its_own_doc_comment_separate_from_the_constants() {
    let source = include_str!("../prefix_admission.rs");
    let lines: Vec<&str> = source.lines().collect();

    let signature = lines
        .iter()
        .position(|line| {
            line.trim_start()
                .starts_with("pub(crate) async fn bind_prefix")
        })
        .expect("bind_prefix's signature line");

    // The nearest non-blank line above the signature must itself be a doc
    // line for `bind_prefix` to render with any documentation at all — a
    // blank line there, with only the constant's declaration further up, is
    // what an undocumented function looks like.
    let nearest_above = (0..signature)
        .rev()
        .map(|index| lines[index].trim())
        .find(|line| !line.is_empty());
    assert_eq!(
        nearest_above.map(|line| line.starts_with("///")),
        Some(true),
        "bind_prefix has no doc comment directly above its signature — the \
         nearest non-blank line was {nearest_above:?}"
    );

    // And the block above the constant must not still carry bind_prefix's
    // own prose, which is what one merged comment looks like from the
    // other end.
    let declaration = lines
        .iter()
        .position(|line| line.trim_start().starts_with("const MAX_PREFIX_PROBES"))
        .expect("the constant's declaration line");
    let const_doc: Vec<&str> = (0..declaration)
        .rev()
        .map(|index| lines[index])
        .take_while(|line| line.trim_start().starts_with("///"))
        .collect();
    assert!(
        !const_doc
            .join("\n")
            .contains("Resolve a cache key to the session holding its history"),
        "the constant's doc block still contains bind_prefix's opening \
         prose — the two comments were never split apart:\n{}",
        const_doc.join("\n")
    );
}
