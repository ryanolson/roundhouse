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
use roundhouse_core::ids::{ResponseId, TurnId};
use roundhouse_core::item::{ItemContent, Role};
use roundhouse_core::store::{MemoryStore, StoreError};
use roundhouse_fleet::{EchoFrontierClient, StaticFrontierCatalog, WireProtocol};

use crate::engine::EngineConfig;

use super::*;

/// The inline size ceiling leaves room for metadata changes, but not an Item.
const RETAINED_ENTRY_CEILING: usize = 64;

/// Bound inline metadata. The ownership and payload-size tests cover heap data.
#[test]
fn one_retained_entry_is_a_fixed_size_whatever_item_it_came_from() {
    assert!(
        std::mem::size_of::<Entry>() <= RETAINED_ENTRY_CEILING,
        "one entry of the projection retains {} bytes, which is a payload \
         rather than a fingerprint",
        std::mem::size_of::<Entry>()
    );
}

/// Reject owned String, Vec, Value, and Arc fields in admission metadata.
/// Copy alone does not exclude borrowed or raw pointers; the field definitions
/// and payload-size test provide the rest of the retention evidence.
#[test]
fn a_projection_retains_no_payload_at_all() {
    fn owns_nothing<T: Copy>() {}
    owns_nothing::<Entry>();
    owns_nothing::<ItemFingerprint>();
    owns_nothing::<ResponseStamp>();

    assert!(
        std::mem::size_of::<ItemFingerprint>() <= RETAINED_ENTRY_CEILING,
        "a fingerprint is {} bytes",
        std::mem::size_of::<ItemFingerprint>()
    );
}

fn user(text: &str) -> Item {
    Item::user_text(text)
}

fn assistant(text: &str) -> Item {
    Item::assistant_text(text, ResponseId::new("resp_1"))
}

/// What a turn claiming `claimed` may append to a session whose committed
/// history is `stored`.
///
/// The production path, both sides fingerprinted exactly as [`bind_prefix`]
/// fingerprints them: the stored side when the log is projected, the claimed
/// side once per request. It returns the delta rather than the agreement
/// boundary because the delta is what a turn runs on, and because every
/// assertion below is about *which items get appended*.
fn suffix(stored_items: &[Item], claimed: &[Item]) -> Option<Vec<Item>> {
    admit(&stored(stored_items.to_vec()), &Claim::of(claimed))
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
        suffix(&stored, &claimed),
        Some(vec![user("again")]),
        "a stamped assistant item must still match the client's copy of it"
    );
}

#[test]
fn a_retry_of_an_answered_turn_yields_nothing_to_append() {
    let stored = vec![user("hello"), assistant("hi")];
    // The retry predates the answer, because the client never saw it.
    assert_eq!(suffix(&stored, &[user("hello")]), Some(Vec::new()));
}

#[test]
fn an_edited_history_is_refused_rather_than_appended() {
    let stored = vec![user("hello"), assistant("hi")];
    assert_eq!(suffix(&stored, &[user("goodbye")]), None);
}

/// A tool call as a pre-M17 log holds it, or as the Messages surface still
/// does: name only, no namespace field.
fn bare_call() -> Item {
    Item::tool_call("call_1", "status", "{}")
}

/// The same call as a post-M17 Responses log holds it.
fn namespaced_call(namespace: &str) -> Item {
    Item::namespaced_tool_call("call_1", "status", Some(namespace.to_string()), "{}")
}

/// **R-N8, half one: a conversation that straddles the change continues.**
///
/// The failure this exists to prevent is the one that would have shipped
/// silently. Every turn of a tool-using session stored before M17 holds
/// `namespace: None`; the client's very next request canonicalizes the same
/// resent call with `Some("mcp__roundhouse")`, because the wire always carried
/// the field and only the log has changed. A comparison that read the namespace
/// as ordinary content would disagree at that item, the search would find no
/// agreement, and the conversation would fork into a fresh generation — while
/// every turn still answered, which is why nothing would go red anywhere else.
///
/// So a stored `None` agrees with any claim, and the suffix is what the client
/// genuinely added.
#[test]
fn a_conversation_stored_before_the_namespace_existed_still_admits_a_namespaced_claim() {
    let stored = vec![user("hello"), bare_call()];
    let claimed = vec![
        user("hello"),
        namespaced_call("mcp__roundhouse"),
        user("and now?"),
    ];
    assert_eq!(
        suffix(&stored, &claimed),
        Some(vec![user("and now?")]),
        "a record written before the field existed must not fork the \
         conversation the day the field lands"
    );
}

/// **R-N8, half two: a stored namespace requires equality.**
///
/// Not blind, and this is what blindness would have cost. A client that
/// re-sends the same tool name under a *different* MCP server is describing a
/// different call — dispatched to a different server, answered by different
/// code — and the log it claims to be continuing does not contain it. That is
/// a changed history, exactly as a changed name is, and the surface's answer to
/// a changed history is to fork rather than to append onto somebody else's
/// conversation.
#[test]
fn a_stored_namespace_disagrees_with_a_different_claimed_one() {
    let stored = vec![user("hello"), namespaced_call("mcp__roundhouse")];
    let claimed = vec![user("hello"), namespaced_call("mcp__other")];
    assert_eq!(
        suffix(&stored, &claimed),
        None,
        "`status` on our server and `status` on somebody else's are two calls"
    );
}

/// **R-N8, half three: and it disagrees with an absent one.**
///
/// The direction the asymmetry does *not* run, pinned because it is the one a
/// reader would expect to be symmetric. A stored `None` agreeing with any claim
/// is a statement about records written before the field existed; it says
/// nothing about a client that stored a namespace and then stopped sending one.
/// Treating that as agreement would make the rule blind in both directions for
/// any conversation whose first turn happened to carry the field, which is the
/// whole of what "not blind" was for.
#[test]
fn a_stored_namespace_disagrees_with_an_absent_claimed_one() {
    let stored = vec![user("hello"), namespaced_call("mcp__roundhouse")];
    let claimed = vec![user("hello"), bare_call()];
    assert_eq!(
        suffix(&stored, &claimed),
        None,
        "a claim that dropped the field is not evidence it means the same call"
    );

    // The control, and it is what proves the two assertions above are about the
    // namespace rather than about tool calls being compared strictly at all:
    // identical records on both sides still admit.
    assert_eq!(
        suffix(&stored, &stored),
        Some(Vec::new()),
        "a verbatim resend of a namespaced call is the ordinary retry"
    );
}

fn configuration(text: &str) -> Item {
    Item {
        role: Role::Developer,
        content: ItemContent::Text { text: text.into() },
        response_id: None,
    }
}

/// A session's committed conversation as the projection holds it, from items a
/// test spells out instead of from a log.
///
/// The fingerprinting is the production one — the items are consumed here
/// exactly as the read loop consumes a batch — so a test written against this
/// is written against what [`stored_conversation`] produces.
fn stored(items: Vec<Item>) -> StoredConversation {
    let configuration_len = turn_configuration_len(&items);
    StoredConversation {
        fingerprints: items.iter().map(ItemFingerprint::of).collect(),
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
    assert_eq!(
        admit(&session, &Claim::of(&claimed)),
        Some(vec![user("again")])
    );

    // Rewritten: the new run leads the delta, ahead of the new history.
    let mut claimed = vec![configuration("v2")];
    claimed.extend_from_slice(&history);
    assert_eq!(
        admit(&session, &Claim::of(&claimed)),
        Some(vec![configuration("v2"), user("again")]),
    );

    // A run that gained a block is a changed run, not a matching prefix.
    let mut claimed = vec![configuration("v1"), configuration("extra")];
    claimed.extend_from_slice(&history);
    assert_eq!(
        admit(&session, &Claim::of(&claimed)),
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
        admit(
            &session,
            &Claim::of(&[configuration("v1"), user("goodbye")])
        ),
        None
    );
    assert_eq!(
        admit(
            &session,
            &Claim::of(&[configuration("v2"), user("goodbye")])
        ),
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
            &Claim::of(&[
                user("hello"),
                Item {
                    role: Role::Assistant,
                    content: ItemContent::Text { text: "hi".into() },
                    response_id: None,
                },
                user("again"),
            ])
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
///
/// [`single_model_catalog`](crate::test_support::single_model_catalog) (M15,
/// H2): this crate's own unit tests are the one audience
/// `tests/common/mod.rs::frontier_catalog` cannot reach, so this is that
/// shared fixture rather than a second hand-rolled copy of it.
fn catalog() -> StaticFrontierCatalog {
    crate::test_support::single_model_catalog(crate::test_support::frontier_spec(
        "anthropic",
        "claude",
        WireProtocol::AnthropicMessages,
    ))
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
        let engine = crate::test_support::engine_over_echo(
            Arc::clone(&store),
            catalog(),
            Arc::new(EchoFrontierClient::new("answer")),
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
            .append_events(&lease, kinds, None)
            .await
            .expect("seed append");
    }

    /// Creates a generation's session and leaves it leased, with nothing
    /// appended — the shape [`probe`] reads back as [`Probe::Busy`]: another
    /// writer's fresh slot, one instruction into its first turn.
    ///
    /// H4's all-busy topology needs this for every generation a search can
    /// reach, so it is named once here rather than repeated at each of nine
    /// call sites.
    async fn hold_busy(&self, generation: u32, node_id: &str) {
        let session_id = self.generation(generation);
        self.store
            .create_session(&session_id, "test-policy")
            .await
            .expect("busy session creation");
        self.store
            .acquire_lease(&session_id, node_id, 60_000)
            .await
            .expect("busy lease request")
            .expect("busy lease granted -- the slot must still be free to hold");
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
        .map(|(session, delta, _)| (session, delta))
    }
}

/// **(a) restart-then-fork.** A store pre-seeded exactly as it would be
/// after a real restart — generation zero holding the pre-restart log,
/// `#g1` holding what the pre-restart process had already forked to — and
/// a node whose counter re-derives generation zero from nothing. A claim
/// that disagrees with generation zero and equals `#g1` (plus one
/// genuinely new turn) must land on `#g1` with only that turn appended.
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
            stored.fingerprints.len(),
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
    rig.conversations
        .commit(&rig.principal, &qualified_key, 0)
        .await;
    let latest_before = rig.conversations.latest(&rig.principal);
    let resolved_before = rig.conversations.resolve(&qualified_key).await.unwrap();

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
        rig.conversations.resolve(&qualified_key).await.unwrap(),
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
    rig.conversations
        .commit(
            &rig.principal,
            &ControlPlane::Open.qualify(&rig.principal, &rig.key),
            2,
        )
        .await;

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

    rig.hold_busy(1, "node-b").await;

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

/// **H4 (M15 hygiene rung): a refusal whose every probed generation was
/// busy must not report zero disagreements.**
///
/// `search` kept one tally, `disagreed`, and [`Probe::Busy`] never
/// incremented it — so a topology where every probe in range answers
/// `Busy` (another writer's slot, F10's shape, at *every* generation
/// rather than one) exhausted with `disagreed: 0` and the refusal read as
/// "the claimed history disagreed with all 0 probed generation(s)", which
/// is not what happened: nine generations were read and every one of them
/// was leased and empty, not disagreeing. The fix counts what the search
/// actually did — probed and found busy — separately from what it probed
/// and found disagreeing, so an operator reading the refusal can tell a
/// client stuck disagreeing with real history from a client colliding with
/// live concurrent writers.
///
/// The topology is F10's, widened from one busy generation to the whole
/// range a search from a fresh hint can reach: generation zero through
/// [`MAX_PREFIX_PROBES`], each created and leased by another node with
/// nothing appended. None of them is `Fresh` (all nine exist), none is
/// `Home` or `Disagrees` (all nine are empty and leased), so the search
/// exhausts having read every one of them and landed on none.
#[tokio::test]
async fn a_refusal_where_every_probe_was_busy_reports_what_it_probed() {
    let rig = Rig::new("acme/ada/all-busy");
    for generation in 0..=MAX_PREFIX_PROBES {
        rig.hold_busy(generation, "other-node").await;
    }

    let error = rig
        .bind(vec![user("anything")])
        .await
        .expect_err("every reachable generation is another writer's, never a home");

    assert_eq!(error.status(), StatusCode::CONFLICT);
    assert_eq!(error.code(), "prefix_admission_exhausted");
    let detail = error
        .detail()
        .expect("a client acting on this refusal needs the tally, not English");
    assert_eq!(
        detail.get("cache_key").and_then(Value::as_str),
        Some(rig.key.as_str())
    );
    assert_eq!(
        detail.get("attempts").and_then(Value::as_u64),
        Some(u64::from(MAX_PREFIX_PROBES) + 1),
        "H4: every generation from the hint through the bound was probed, \
         whether or not any of them disagreed -- the total must not \
         collapse to the disagreement count alone"
    );
    assert_eq!(
        detail.get("disagreed").and_then(Value::as_u64),
        Some(0),
        "H4: nothing here disagreed -- every probe found the slot busy, \
         and the defect this guards is exactly that a reader could not \
         tell that apart from 'nothing was probed at all'"
    );
    assert_eq!(
        detail.get("busy").and_then(Value::as_u64),
        Some(u64::from(MAX_PREFIX_PROBES) + 1),
        "H4: the refusal must say what it actually found busy rather than \
         folding it into a silent zero"
    );
}

/// F4 (M15 review) refutation, part (a): the F8 topology (nine generations
/// disagreeing, none busy) checked against the `disagreed`/`busy` split
/// H4 added, not just `attempts`.
///
/// F8's own test only reads `attempts`, so a mutant that folds
/// [`Probe::Disagrees`] into the `busy` counter (or that builds the
/// refusal as `disagreed: 0, busy: attempts`) still satisfies it: nine
/// disagreements report `attempts == 9` either way. This test is the same
/// topology with the two tallies read individually, which the fold cannot
/// satisfy — under the real code both are visible here; under either
/// described mutant this would read `disagreed == 0, busy == 9` instead.
#[tokio::test]
async fn f4_a_refusal_where_every_probe_disagreed_reports_disagreed_not_busy() {
    let rig = Rig::new("acme/ada/f4-all-disagree");
    for generation in 0..=MAX_PREFIX_PROBES {
        rig.seed(generation, vec![user(&format!("generation {generation}"))])
            .await;
    }

    let error = rig
        .bind(vec![user("none of the above")])
        .await
        .expect_err("every generation the search can reach disagrees");

    let detail = error
        .detail()
        .expect("a client acting on this refusal needs the tally, not English");
    assert_eq!(
        detail.get("attempts").and_then(Value::as_u64),
        Some(u64::from(MAX_PREFIX_PROBES) + 1),
    );
    assert_eq!(
        detail.get("disagreed").and_then(Value::as_u64),
        Some(u64::from(MAX_PREFIX_PROBES) + 1),
        "F4: every probe here disagreed and none was busy -- a mutant \
         folding Probe::Disagrees into the busy counter would report 0 \
         here, and F8's own test (which never reads this field) would not \
         catch it"
    );
    assert_eq!(
        detail.get("busy").and_then(Value::as_u64),
        Some(0),
        "F4: nothing here was busy -- see disagreed's assertion above"
    );
}

/// F4 (M15 review) refutation, part (b): the downward walk's own
/// `Probe::Busy => busy += 1` arm (`prefix_admission.rs:377`), reached only
/// when the search's hint is already above zero.
///
/// Every other busy-topology test in this suite binds from a fresh `Rig`,
/// whose hint is 0 — so the downward walk's `current.checked_sub(step)`
/// fails on step 1 and the loop body never runs at all, regardless of what
/// its `Probe::Busy` arm does. Here the hint is walked to generation 4
/// first (via `Conversations::commit`, the same seam the P1 test above
/// uses to move a node's counter without a real turn), and generations
/// 0..=12 are all held busy by another node: 4 downward
/// (3, 2, 1, 0), the current generation itself, and 8 upward (5..=12) is
/// every reachable generation, all of them busy. If the arm at :377 were
/// removed (or its `Probe::Busy` case were a no-op), the four generations
/// this walk reaches downward would silently drop out of both tallies and
/// `busy` would read 9, not 13.
#[tokio::test]
async fn f4_the_downward_walks_busy_arm_is_reached_from_a_nonzero_hint() {
    let rig = Rig::new("acme/ada/f4-downward-busy");
    let key = ControlPlane::Open.qualify(&rig.principal, &rig.key);
    rig.conversations.commit(&rig.principal, &key, 4).await;

    for generation in 0..=(4 + MAX_PREFIX_PROBES) {
        rig.hold_busy(generation, "other-node").await;
    }

    let error = rig
        .bind(vec![user("anything")])
        .await
        .expect_err("every generation this walk can reach, in both directions, is busy");

    let detail = error
        .detail()
        .expect("a client acting on this refusal needs the tally, not English");
    assert_eq!(
        detail.get("attempts").and_then(Value::as_u64),
        Some(u64::from(4 + MAX_PREFIX_PROBES) + 1),
        "F4: 4 downward (3,2,1,0) + 1 current (4) + 8 upward (5..=12) = 13 \
         generations probed, every one of them busy"
    );
    assert_eq!(
        detail.get("disagreed").and_then(Value::as_u64),
        Some(0),
        "F4: nothing here disagreed -- every probe found the slot busy"
    );
    assert_eq!(
        detail.get("busy").and_then(Value::as_u64),
        Some(u64::from(4 + MAX_PREFIX_PROBES) + 1),
        "F4: including the 4 the downward walk itself reached below the \
         hint -- the arm this test exists to reach"
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
    rig.conversations
        .commit(
            &rig.principal,
            &ControlPlane::Open.qualify(&rig.principal, &rig.key),
            1,
        )
        .await;

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

// `Delegating` is deliberately not `use`d in this file: fixtures throughout
// call methods directly on a concrete double (`store.create_session(..)`),
// and having both traits' same-named methods in scope at once would make
// those calls ambiguous (E0034). Fully qualifying the trait here avoids that
// without pushing disambiguation onto every call site instead.
#[async_trait::async_trait]
impl roundhouse_core::store::doubles::Delegating for CountingStore {
    type Backend = MemoryStore;

    fn backend(&self) -> &MemoryStore {
        &self.inner
    }

    async fn is_leased(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        self.inner.is_leased(session_id).await
    }

    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
        self.create_session_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.create_session(session_id, model_policy).await
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

#[tokio::test]
async fn rewrite_signal_distinguishes_divergence_from_busy_and_resumed_generations() {
    for scenario in ["fresh", "rewrite", "busy", "resume"] {
        let rig = Rig::new(scenario);
        let claimed = vec![Item::user_text("current history")];
        match scenario {
            "rewrite" => {
                rig.seed(0, vec![Item::user_text("different history")])
                    .await
            }
            "busy" => rig.hold_busy(0, "other-node").await,
            "resume" => {
                rig.seed(0, vec![Item::user_text("different history")])
                    .await;
                rig.seed(1, claimed.clone()).await;
            }
            _ => {}
        }
        let (session, _, history_rewritten) = bind_prefix(
            &rig.engine,
            rig.store.as_ref(),
            &rig.conversations,
            &ControlPlane::Open,
            &rig.principal,
            &rig.key,
            claimed,
        )
        .await
        .expect(scenario);
        assert_eq!(history_rewritten, scenario == "rewrite", "{scenario}");
        assert_eq!(session, rig.generation(u32::from(scenario != "fresh")));
    }
}

// -----------------------------------------------------------------------
// The comparison itself: one fingerprint per item, and the relation it has
// to agree with (M18)
// -----------------------------------------------------------------------

/// The item comparison **as it stood before the fingerprint**, kept here as the
/// specification the fingerprint is checked against.
///
/// This is the deleted code, verbatim, and that is the point of it: the rung's
/// whole claim is that no conversation changes its mind about where it
/// continues, and the only way to state that claim as a test is to keep the
/// relation it must reproduce and compare the two over a universe wide enough
/// to separate them. A reference written afresh from the prose would be a test
/// of the prose.
///
/// It is not a second production path: nothing outside this module can reach
/// it, and [`a_fingerprint_agrees_exactly_where_the_structural_comparison_did`]
/// is its only caller.
mod reference {
    use super::{Item, ItemContent};

    pub(super) fn same_item(stored: &Item, claimed: &Item) -> bool {
        stored.role == claimed.role && same_content(&stored.content, &claimed.content)
    }

    fn same_content(stored: &ItemContent, claimed: &ItemContent) -> bool {
        match (stored, claimed) {
            (
                ItemContent::ToolCall {
                    call_id: stored_id,
                    name: stored_name,
                    arguments: stored_arguments,
                    namespace: stored_namespace,
                },
                ItemContent::ToolCall {
                    call_id: claimed_id,
                    name: claimed_name,
                    arguments: claimed_arguments,
                    namespace: claimed_namespace,
                },
            ) => {
                stored_id == claimed_id
                    && stored_name == claimed_name
                    && stored_arguments == claimed_arguments
                    && same_namespace(stored_namespace.as_deref(), claimed_namespace.as_deref())
            }
            _ => stored == claimed,
        }
    }

    fn same_namespace(stored: Option<&str>, claimed: Option<&str>) -> bool {
        match stored {
            None => true,
            Some(_) => stored == claimed,
        }
    }
}

fn call(call_id: &str, name: &str, arguments: &str, namespace: Option<&str>) -> ItemContent {
    ItemContent::ToolCall {
        call_id: call_id.into(),
        name: name.into(),
        arguments: arguments.into(),
        namespace: namespace.map(str::to_owned),
    }
}

fn opaque(block_type: &str, block: Value) -> ItemContent {
    ItemContent::Opaque {
        block_type: block_type.into(),
        block,
    }
}

/// Every content shape, crossed with every role and with three response
/// stamps.
///
/// Shared by the two tests that need a universe, so that widening it widens
/// both: the fingerprint's agreement with the comparison it replaced, and
/// admission's agreement with the chain.
///
/// What is deliberately in it, beyond one of each variant:
///
/// - **The three namespace spellings of one call** (absent, ours, somebody
///   else's), which is the only asymmetric rule in the relation.
/// - **A call differing in each of its other three fields**, so the namespace
///   agreement cannot be mistaken for tool calls not being compared at all.
/// - **Two numeric spellings of one opaque block.** `1` and `1.0` are
///   different `serde_json` numbers; a digest taken over a `Display` of the
///   block gets this one right, and a digest that read every number as `f64`
///   would not.
/// - **One opaque block written in two key orders.** Both rows end up as the
///   *same* [`Value`] — with `serde_json`'s `preserve_order` off, a parsed
///   object is a `BTreeMap` and cannot record the order it arrived in — so
///   this pair cannot fail here, and says so: the reordering a chained Relay
///   does is normalized before admission ever sees it. The sort inside the
///   digest is the guard for the day that feature flips, which no test can
///   reach while it is off.
/// - **A text item whose text is exactly another item's render.** This is why
///   the chain's own item digest could not be reused as the fingerprint: the
///   render of this pair is identical, and admission must still call them two
///   different items.
fn comparison_universe() -> Vec<Item> {
    let contents = [
        ItemContent::Text { text: "a".into() },
        ItemContent::Text { text: "b".into() },
        call("c1", "ls", r#"{"path":"."}"#, None),
        call("c1", "ls", r#"{"path":"."}"#, Some("mcp__roundhouse")),
        call("c1", "ls", r#"{"path":"."}"#, Some("mcp__other")),
        call("c2", "ls", r#"{"path":"."}"#, None),
        call("c1", "cat", r#"{"path":"."}"#, None),
        call("c1", "ls", r#"{"path":"src"}"#, None),
        // The same characters, split differently between the id and the name.
        // Two different calls, and a digest that concatenated its fields
        // without their lengths would call them one.
        call("c1l", "s", r#"{"path":"."}"#, None),
        ItemContent::ToolResult {
            call_id: "c1".into(),
            output: "a.rs".into(),
        },
        ItemContent::Thinking {
            thinking: "hmm".into(),
            signature: "sig".into(),
        },
        // Same reasoning, different signature: a different block upstream.
        ItemContent::Thinking {
            thinking: "hmm".into(),
            signature: "other".into(),
        },
        ItemContent::RedactedThinking {
            data: "opaque".into(),
        },
        opaque(
            "image",
            serde_json::json!({"type": "image", "source": {"data": "AAAA"}}),
        ),
        // The same body under a different block type.
        opaque(
            "document",
            serde_json::json!({"type": "image", "source": {"data": "AAAA"}}),
        ),
        opaque("counter", serde_json::json!({"count": 1, "ratio": 2})),
        opaque("counter", serde_json::json!({"count": 1.0, "ratio": 2})),
        opaque("counter", serde_json::json!({"count": -1, "ratio": 2})),
        opaque(
            "ordered",
            serde_json::json!({"a": [1, {"x": null}], "b": true, "c": "s"}),
        ),
        opaque(
            "ordered",
            serde_json::json!({"c": "s", "b": true, "a": [1, {"x": null}]}),
        ),
        // The render-collision pair: this text renders exactly as the
        // `call("c1", "ls", …)` item above does.
        ItemContent::Text {
            text: r#"<tool_call id="c1" name="ls">{"path":"."}</tool_call>"#.into(),
        },
    ];
    let roles = [
        Role::System,
        Role::Developer,
        Role::User,
        Role::Assistant,
        Role::Tool,
    ];
    let stamps = [
        None,
        Some(ResponseId::new("resp_1")),
        Some(ResponseId::new("resp_2")),
    ];
    let mut universe = Vec::new();
    for role in roles {
        for content in &contents {
            for stamp in &stamps {
                universe.push(Item {
                    role,
                    content: content.clone(),
                    response_id: stamp.clone(),
                });
            }
        }
    }
    universe
}

/// **The fingerprint admits exactly what the structural comparison admitted —
/// over every variant, every role, every stamp and both namespace
/// directions.**
///
/// This is the rung's load-bearing test. Replacing the items with digests
/// changes nothing a client can see *only if* the relation is preserved
/// exactly, and the failure of getting it wrong is silent in the worst way: a
/// relation that is accidentally stricter forks warm conversations while every
/// turn still answers, and one that is accidentally looser admits a claim onto
/// history it does not contain. Neither shows up in a test that only checks the
/// cases somebody remembered.
///
/// So the assertion is equality of the two relations, in both directions, for
/// every ordered pair of a universe built to separate them —
/// [`comparison_universe`] says what is in it and why. The counters at the end
/// are what stop this passing vacuously: a relation that agreed with nothing,
/// or that only ever agreed with an item and itself, would satisfy the
/// comparison above and prove nothing about the two rules that make agreement
/// looser than equality.
#[test]
fn a_fingerprint_agrees_exactly_where_the_structural_comparison_did() {
    let mut universe = comparison_universe();
    // **`0.0` and `-0.0` are one number to `Value`'s `PartialEq`**, so
    // admission has always admitted a claim that respelled a zero, and the
    // fingerprint must too. They live here rather than in
    // `comparison_universe` because the *chain* disagrees with admission on
    // this pair — its digest is over a `Display`, where the sign survives —
    // and `admission_agreement_implies_equal_chain_links` would fail on it.
    // That divergence predates this rung and is not widened by it; see this
    // module's note on the chain test.
    universe.push(Item {
        role: Role::User,
        content: opaque("counter", serde_json::json!({"zero": 0.0})),
        response_id: None,
    });
    universe.push(Item {
        role: Role::User,
        content: opaque("counter", serde_json::json!({"zero": -0.0})),
        response_id: None,
    });

    // Fingerprinted once per item rather than once per pair: the relation is a
    // property of the values, and the whole universe is squared below.
    let fingerprints: Vec<ItemFingerprint> = universe.iter().map(ItemFingerprint::of).collect();

    let json_of = |item: &Item| serde_json::to_string(item).expect("an item serializes");
    let (mut agreed, mut disagreed) = (0usize, 0usize);
    let (mut across_stamps, mut across_namespaces, mut across_respellings) =
        (0usize, 0usize, 0usize);
    for (stored_index, stored) in universe.iter().enumerate() {
        for (claimed_index, claimed) in universe.iter().enumerate() {
            let expected = reference::same_item(stored, claimed);
            let actual = fingerprints[stored_index].matches(&fingerprints[claimed_index]);
            assert_eq!(
                actual, expected,
                "the fingerprint and the comparison it replaces disagree about \
                 whether a session holding\n  {stored:?}\nis continued by a claim of\n  \
                 {claimed:?}"
            );
            if !expected {
                disagreed += 1;
                continue;
            }
            agreed += 1;
            if stored_index != claimed_index {
                across_stamps += usize::from(stored.response_id != claimed.response_id);
                across_namespaces += usize::from(matches!(
                    (&stored.content, &claimed.content),
                    (
                        ItemContent::ToolCall {
                            namespace: None,
                            ..
                        },
                        ItemContent::ToolCall {
                            namespace: Some(_),
                            ..
                        }
                    )
                ));
                // Two items that agree while the bytes the log holds them as
                // differ, for a reason that is not the stamp and not the
                // namespace: the signed-zero row, and nothing else in this
                // universe. Measured on the serialized form because a
                // respelled zero is *equal* as a `Value` — which is the whole
                // reason admission admits it — so comparing contents here
                // would count nothing.
                across_respellings += usize::from(
                    stored.role == claimed.role
                        && stored.response_id == claimed.response_id
                        && !matches!(stored.content, ItemContent::ToolCall { .. })
                        && json_of(stored) != json_of(claimed),
                );
            }
        }
    }

    assert!(disagreed > 0, "nothing in the universe disagreed");
    assert!(
        agreed > universe.len(),
        "only an item and itself ever agreed, so neither of the two rules that \
         make agreement looser than equality was exercised"
    );
    assert!(
        across_stamps > 0,
        "no agreeing pair differed in its response stamp"
    );
    assert!(
        across_namespaces > 0,
        "no agreeing pair was a stored call with no namespace against a \
         claimed one that has one -- the asymmetric half of the rule went \
         untested"
    );
    assert!(
        across_respellings > 0,
        "no agreeing pair differed in how its JSON spelled one number -- the \
         signed-zero row did not land, so nothing here proved the fingerprint \
         follows JSON equality rather than the bytes"
    );
}

/// **The four namespace directions, named, with the delta each one produces.**
///
/// The matrix test above compares the fingerprint against the relation it
/// replaces, so a mutation that broke *both* would satisfy it. This states the
/// rule directly instead, in literals: three of the four directions agree or
/// disagree because M17 (R-N8) says so, and the fourth is the control.
#[test]
fn the_namespace_rule_runs_in_one_direction_only() {
    let directions = [
        // (stored, claimed, agrees)
        (None, None, true),
        (None, Some("mcp__roundhouse"), true),
        (Some("mcp__roundhouse"), Some("mcp__roundhouse"), true),
        (Some("mcp__roundhouse"), Some("mcp__other"), false),
        (Some("mcp__roundhouse"), None, false),
    ];
    for (stored_namespace, claimed_namespace, agrees) in directions {
        let stored_item = Item {
            role: Role::Assistant,
            content: call("c1", "status", "{}", stored_namespace),
            response_id: None,
        };
        let claimed_item = Item {
            role: Role::Assistant,
            content: call("c1", "status", "{}", claimed_namespace),
            response_id: None,
        };
        assert_eq!(
            ItemFingerprint::of(&stored_item).matches(&ItemFingerprint::of(&claimed_item)),
            agrees,
            "stored {stored_namespace:?} against claimed {claimed_namespace:?}"
        );
    }
}

/// **An opaque block's numbers agree exactly where JSON equality does.**
///
/// Stated against [`Value`]'s own `==` and against literals rather than against
/// the reference relation, because this is the arm a plausible implementation
/// gets wrong in both directions at once: a digest over the block's `Display`
/// splits `0.0` from `-0.0`, and a digest that read every number through
/// `as_f64` merges `1` with `1.0`. Each of those is a silent fork or a silent
/// admission, and the reference relation would not catch either if the mistake
/// were made twice.
#[test]
fn an_opaque_blocks_numbers_agree_exactly_where_json_equality_does() {
    let cases = [
        (serde_json::json!(1), serde_json::json!(1.0), false),
        (serde_json::json!(0.0), serde_json::json!(-0.0), true),
        (serde_json::json!(1), serde_json::json!(1), true),
        (serde_json::json!(-1), serde_json::json!(1), false),
        (serde_json::json!(100), serde_json::json!(1.0e2), false),
        (serde_json::json!(1.5), serde_json::json!(1.5), true),
        (serde_json::json!(1), serde_json::json!("1"), false),
        (serde_json::json!(0), serde_json::json!(false), false),
        (serde_json::json!(null), serde_json::json!(0), false),
    ];
    for (left, right, agrees) in cases {
        let block = |value: &Value| Item {
            role: Role::User,
            content: opaque("counter", serde_json::json!({"n": value})),
            response_id: None,
        };
        assert_eq!(
            left == right,
            agrees,
            "the fixture's own expectation disagrees with serde_json: \
             {left} vs {right}"
        );
        assert_eq!(
            ItemFingerprint::of(&block(&left)).matches(&ItemFingerprint::of(&block(&right))),
            agrees,
            "{left} against {right}"
        );
    }
}

/// **Key order inside an opaque block is not content, at any depth.**
///
/// Synergy ruling S3's first chain hazard, end to end: a chained NeMo Relay
/// re-serializes intercepted bodies through an alphabetizing map, so the second
/// turn of a conversation behind one arrives with every object key reordered.
/// This drives both spellings from the *text* a client would send, which is the
/// only place the order exists — with `preserve_order` off, parsing normalizes
/// it into a `BTreeMap` and the two become one `Value`.
///
/// So this passes today for a reason one layer below the digest, and that is
/// worth stating rather than implying: it is a guard on the parse-then-compare
/// pipeline, not on the sort inside [`fingerprint`]. Nothing can test that sort
/// while the feature it defends against is off; what it buys is that flipping
/// `preserve_order` somewhere in the dependency graph does not silently fork
/// every Relay-chained session.
#[test]
fn an_opaque_block_fingerprints_the_same_in_any_key_order() {
    let sent = r#"{"type":"image","source":{"type":"base64","data":"AA"},"index":2}"#;
    let relayed = r#"{"index":2,"source":{"data":"AA","type":"base64"},"type":"image"}"#;
    let block = |json: &str| Item {
        role: Role::User,
        content: opaque(
            "image",
            serde_json::from_str(json).expect("the fixture is JSON"),
        ),
        response_id: None,
    };

    assert!(
        ItemFingerprint::of(&block(sent)).matches(&ItemFingerprint::of(&block(relayed))),
        "a re-encoded body must still continue the conversation it belongs to"
    );
    // The control: a changed payload still disagrees, so the insensitivity
    // above is to key order and not to the body.
    let edited = r#"{"type":"image","source":{"type":"base64","data":"AB"},"index":2}"#;
    assert!(
        !ItemFingerprint::of(&block(sent)).matches(&ItemFingerprint::of(&block(edited))),
        "a changed block must fork"
    );
}

/// Session and sequence identity, M1: wherever admission calls two items the
/// same, the chain must too.
///
/// The chain (`roundhouse_core::item::chain`) is what placement will look a
/// conversation up by, and admission is what decides the conversation is one.
/// If the two disagreed — admission agreeing on a pair whose links differ — a
/// continuation admission keeps on its generation would be looked up under a
/// tip nobody stored and placed as a new sequence, on a cold deployment, with
/// every turn still answering. The comparison is private to this module, which
/// is why this lives beside it and not in core.
///
/// The item digests are compared through the chain rather than directly: both
/// chains share link 0, so their link 1 is equal exactly when the two item
/// digests are, and the digest itself is private to core so it cannot leak.
///
/// The two rules that make agreement looser than equality are exactly the
/// ones the render must not see: the response stamp (never compared) and the
/// namespace (a stored `None` agrees with any claim). The universe crosses
/// every role with stamps and namespaces on every content shape, and the counts
/// at the end prove both rules were actually exercised rather than vacuously
/// true.
///
/// **One known divergence, which predates this rung and is not widened by it**:
/// `0.0` and `-0.0` inside an opaque block are one number to admission (JSON
/// equality says so) and two to the chain (its digest is over a `Display`,
/// where the sign survives). The signed-zero pair therefore lives in
/// [`a_fingerprint_agrees_exactly_where_the_structural_comparison_did`] and not
/// in [`comparison_universe`]. Closing it means deciding which side is right —
/// whether a respelled zero is a new lineage — and that is a design question
/// about placement, not a fingerprint bug.
#[test]
fn admission_agreement_implies_equal_chain_links() {
    use roundhouse_core::item::chain::Chain;

    let universe = comparison_universe();
    let fingerprints: Vec<ItemFingerprint> = universe.iter().map(ItemFingerprint::of).collect();

    // A shared earlier item, so the comparison is of a link that extends a
    // predecessor and not only of a first link.
    let before = Item::user_text("before");
    let chain_of = |item: &Item| Chain::over(&[before.clone(), item.clone()]);
    let (mut agreed, mut across_stamps, mut across_namespaces) = (0, 0, 0);
    for (stored_index, stored) in universe.iter().enumerate() {
        for (claimed_index, claimed) in universe.iter().enumerate() {
            if !fingerprints[stored_index].matches(&fingerprints[claimed_index]) {
                continue;
            }
            agreed += 1;
            across_stamps += usize::from(stored.response_id != claimed.response_id);
            across_namespaces += usize::from(stored.content != claimed.content);
            assert!(
                chain_of(stored).links() == chain_of(claimed).links(),
                "admission agrees on {stored:?} and {claimed:?}, their chain links do not"
            );
        }
    }
    assert!(
        agreed > universe.len(),
        "only reflexive agreement was exercised"
    );
    assert!(
        across_stamps > 0,
        "no agreeing pair differed in its response stamp"
    );
    assert!(
        across_namespaces > 0,
        "no agreeing pair differed in its namespace"
    );
}

// -----------------------------------------------------------------------
// What the projection retains, measured rather than argued (M18)
// -----------------------------------------------------------------------

/// The heap one projection holds.
///
/// The vector of fingerprints and nothing else, which is the claim:
/// [`a_projection_retains_no_payload_at_all`] is what makes that exhaustive
/// rather than optimistic — a `Copy` element cannot own a second allocation for
/// this to be missing.
fn retained_bytes(stored: &StoredConversation) -> usize {
    stored.fingerprints.capacity() * std::mem::size_of::<ItemFingerprint>()
}

/// An item whose payload is `bytes` long, in the one variant that has no bound
/// on its size.
fn pasted_image(bytes: usize) -> Item {
    Item {
        role: Role::User,
        content: opaque(
            "image",
            serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "A".repeat(bytes) },
            }),
        ),
        response_id: None,
    }
}

/// **The projection's retained bytes do not move with the payload, end to end
/// through a real store.**
///
/// The `size_of` assertions above are about the type; this is the property
/// those types exist for, measured on two sessions that differ only in how
/// much payload they hold. A megabyte of base64 — one pasted screenshot — is
/// the realistic case, and it used to be retained in full for every generation
/// the search probed.
#[tokio::test]
async fn the_retained_projection_does_not_grow_with_the_payload() {
    const PAYLOAD: usize = 1 << 20;

    let history = |bytes| {
        vec![
            user("look at this"),
            pasted_image(bytes),
            assistant("I see"),
        ]
    };
    let small = Rig::new("acme/ada/retained-small");
    small.seed(0, history(2)).await;
    let large = Rig::new("acme/ada/retained-large");
    large.seed(0, history(PAYLOAD)).await;

    let small = stored_conversation(small.store.as_ref(), &small.generation(0))
        .await
        .expect("the payload-free generation projects");
    let large = stored_conversation(large.store.as_ref(), &large.generation(0))
        .await
        .expect("the megabyte-carrying generation projects");

    assert_eq!(large.fingerprints.len(), small.fingerprints.len());
    assert_eq!(
        retained_bytes(&large),
        retained_bytes(&small),
        "the projection of a megabyte-carrying session retains more than the \
         projection of the same conversation without the megabyte"
    );
    assert!(
        retained_bytes(&large) < PAYLOAD / 1000,
        "three items retained {} bytes against a {PAYLOAD}-byte payload",
        retained_bytes(&large)
    );
    // And it is still the same conversation: the claim the client resends,
    // payload included, continues it.
    assert_eq!(
        admit(&large, &Claim::of(&history(PAYLOAD))),
        Some(Vec::new()),
        "a verbatim resend of the stored history is the ordinary retry"
    );
    assert_eq!(
        admit(&large, &Claim::of(&history(PAYLOAD - 1))),
        None,
        "and a claim whose image differs by one byte is a different \
         conversation -- a digest that ignored the payload would admit it"
    );
}

/// A long conversation, as a client resends it: no response stamps anywhere,
/// and the tool call canonicalized with the namespace its wire always carried.
///
/// Both of those are the ordinary shape of a resend rather than decoration —
/// the client has no field to put a stamp in, and a Codex client sends the
/// namespace beside the name — so a long-history test that left them out would
/// be testing a claim no client makes.
fn resent(history: &[Item]) -> Vec<Item> {
    history
        .iter()
        .map(|item| Item {
            role: item.role,
            content: match &item.content {
                ItemContent::ToolCall {
                    call_id,
                    name,
                    arguments,
                    namespace: None,
                } => call(call_id, name, arguments, Some("mcp__roundhouse")),
                other => other.clone(),
            },
            response_id: None,
        })
        .collect()
}

/// `turns` turns of a tool-using conversation, with one pasted image of
/// `image_bytes` in it.
///
/// Long enough to page the projection's read loop more than once
/// ([`READ_BATCH`] is 256). The image size is a parameter so the same
/// conversation can be built twice, differing only in payload, which is what
/// makes the retained-bytes comparison at the end a measurement rather than an
/// assertion about one number.
fn long_history(turns: usize, image_bytes: usize) -> Vec<Item> {
    let mut history = vec![configuration("v1")];
    for turn in 0..turns {
        history.push(user(&format!("turn {turn}")));
        history.push(Item::tool_call(
            format!("call_{turn}"),
            "grep",
            format!(r#"{{"pattern":"{turn}"}}"#),
        ));
        history.push(Item {
            role: Role::Tool,
            content: ItemContent::ToolResult {
                call_id: format!("call_{turn}"),
                output: format!("{turn} hits"),
            },
            response_id: None,
        });
        history.push(assistant(&format!("answer {turn}")));
    }
    history.insert(turns * 2, pasted_image(image_bytes));
    history
}

/// **A real client's full-history request, at length: the delta is exactly the
/// new turn, and what admission kept to decide that is a few kilobytes.**
///
/// The integration the milestone is for. Every other test in this file works on
/// a handful of items, where retaining the conversation and retaining a digest
/// of it are indistinguishable; this one drives [`bind_prefix`] over a history
/// that pages the read loop, carries a pasted image, and straddles the
/// namespace change, and asserts the three things that must all hold at once:
///
/// 1. the claim lands on the generation it has been using, with only the
///    genuinely new turn in the delta — not the whole history re-appended;
/// 2. a rewritten configuration run is still recorded rather than forked on,
///    at this length too;
/// 3. an edit *inside* the history still forks, so the agreement in (1) is not
///    a long history being waved through.
///
/// And then what the search kept to decide all three: **a fixed cost per item,
/// not per byte.** Stated as the comparison against the same conversation
/// carrying a two-byte image rather than as a ratio — a long conversation of
/// *small* items legitimately projects to a sizeable fraction of its own JSON,
/// because a fingerprint is a few dozen bytes and so is a short item. The claim
/// was never "the projection is small relative to the transcript"; it is that
/// the transcript's size does not enter into it.
#[tokio::test]
async fn a_long_full_history_request_admits_only_its_new_turn() {
    const TURNS: usize = 80;
    const IMAGE: usize = 1 << 20;

    let rig = Rig::new("acme/ada/long-history");
    let history = long_history(TURNS, IMAGE);
    let payload_bytes: usize = history
        .iter()
        .map(|item| {
            serde_json::to_string(item)
                .expect("an item serializes")
                .len()
        })
        .sum();
    rig.seed(0, history.clone()).await;

    let mut claimed = resent(&history);
    claimed.push(user("one more thing"));
    let (session_id, delta) = rig
        .bind(claimed)
        .await
        .expect("the client's own history continues the session it came from");
    assert_eq!(session_id, rig.generation(0));
    assert_eq!(
        delta,
        vec![user("one more thing")],
        "a {TURNS}-turn history was re-appended onto itself"
    );

    // (2) The same claim with a rewritten instruction block: recorded at the
    // head, not forked on.
    let mut rewritten = resent(&history);
    rewritten[0] = configuration("v2");
    rewritten.push(user("one more thing"));
    let (session_id, delta) = rig
        .bind(rewritten)
        .await
        .expect("a rewritten configuration run is not a changed history");
    assert_eq!(session_id, rig.generation(0));
    assert_eq!(delta, vec![configuration("v2"), user("one more thing")]);

    // (3) An edit deep inside the history forks, and the fork is the proof
    // that (1) compared the whole prefix rather than its ends.
    let mut edited = resent(&history);
    edited[TURNS] = user("something the session never saw");
    let (forked, _) = rig
        .bind(edited)
        .await
        .expect("a divergent claim opens a generation of its own");
    assert_eq!(forked, rig.generation(1));

    // And the state the search kept to decide all three.
    let projected = stored_conversation(rig.store.as_ref(), &rig.generation(0))
        .await
        .expect("the long generation projects");
    assert_eq!(projected.fingerprints.len(), history.len());
    assert!(
        retained_bytes(&projected) <= history.len() * RETAINED_ENTRY_CEILING,
        "the projection retained {} bytes for {} items, which is more than a \
         fingerprint each",
        retained_bytes(&projected),
        history.len()
    );

    // The same conversation without the megabyte: the projection is the same
    // size, which is the claim. A ratio against `payload_bytes` would pass
    // here for the wrong reason — this history is a megabyte of image and
    // ninety kilobytes of short items, so the fingerprints are a tiny
    // fraction of it either way.
    let without = Rig::new("acme/ada/long-history-small");
    without.seed(0, long_history(TURNS, 2)).await;
    let without = stored_conversation(without.store.as_ref(), &without.generation(0))
        .await
        .expect("the payload-free twin projects");
    assert_eq!(
        retained_bytes(&projected),
        retained_bytes(&without),
        "a {payload_bytes}-byte conversation and the same conversation \
         without its image must project to the same retained size"
    );
}
