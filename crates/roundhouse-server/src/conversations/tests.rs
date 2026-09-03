// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`conversations`](super)'s unit tests, in their own file for the reason
//! the crate's other large modules (`prefix_admission`, `mcp_api`,
//! `claude_launch`, `relay_handoff`, `control_config::directory`,
//! `control_config::config`) already are: M14.1's durable maps and their
//! node-local memo earned this file a wider suite than the resolver itself,
//! and a module that keeps growing to hold it is not where the next reader
//! looks first for what a client's own name for a conversation resolves to
//! (M14.1 review, F3).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

fn ada() -> Principal {
    Principal::new("acme", "ada")
}

fn bob() -> Principal {
    Principal::new("globex", "bob")
}

/// Two `Conversations` over one set of maps: the deployment this rung
/// exists for, expressed without a Redis.
///
/// A shared [`MemoryCorrelationMaps`] is the same seam a shared Redis is —
/// one store, two nodes' vocabularies over it — and it isolates what this
/// file is responsible for from what the backend is. The Redis half of the
/// same claim is `tests/correlation_any_node.rs`, gated on a real server.
fn two_nodes() -> (Conversations, Conversations) {
    let maps = Arc::new(MemoryCorrelationMaps::new());
    (
        Conversations::over(Arc::clone(&maps) as Arc<dyn CorrelationMaps>),
        Conversations::over(maps),
    )
}

#[tokio::test]
async fn a_reader_and_a_turn_resolve_one_cache_key_to_one_session() {
    // The whole reason these maps are shared rather than owned by the
    // Responses surface: an overlay installed against `resolve`'s answer
    // has to reach the session `bind` hands the engine, generation and all.
    let conversations = Conversations::new();
    let key = "acme/ada/main";

    // F9 (M12.1 review): before a turn has bound it, the key names nothing
    // — not generation zero, which is a real session id in the shared
    // store the moment any node mints it.
    assert_eq!(
        conversations.resolve(key).await.unwrap(),
        None,
        "a reader with no binding must say so rather than mint the id a \
         first turn would have minted"
    );

    assert_eq!(
        Some(conversations.bind(&ada(), key).await),
        conversations.resolve(key).await.unwrap()
    );

    let forked = conversations.fork(&ada(), key).await;
    assert_eq!(forked.as_str(), "acme/ada/main#g1");
    assert_eq!(
        conversations.resolve(key).await.unwrap(),
        Some(forked.clone()),
        "a rebound key stays rebound: a reader that kept answering \
         generation zero would narrow a session no turn will run in"
    );
    assert_eq!(conversations.bind(&ada(), key).await, forked);
}

/// R-M2 (M12): a tool-use id names one session exactly, and binding one is
/// not a claim about who is working where.
///
/// The correlation semantics themselves — the partition by principal, the
/// unknown and foreign ids — are the shared contract's now and are
/// asserted against both backends in
/// `roundhouse_core::control::correlation::contract`. What is *this*
/// type's is the second assertion: `latest` does not move.
#[tokio::test]
async fn binding_a_tool_call_names_its_session_without_moving_latest() {
    let conversations = Conversations::new();
    let subagent = conversations.bind(&ada(), "acme/ada/sub").await;
    let parent = conversations.bind(&ada(), "acme/ada/main").await;
    conversations
        .bind_call(&ada(), "toolu_sub", subagent.clone())
        .await;

    assert_eq!(
        conversations
            .session_of_call(&ada(), "toolu_sub")
            .await
            .unwrap(),
        Some(subagent),
        "the session that emitted the call is the session the answer to it \
         concerns, whatever else the principal has been doing since"
    );

    // Binding a call is not a claim that the principal is now working in
    // that conversation — the very race this exists to remove would come
    // straight back if a subagent's tool call moved its parent's `latest`.
    assert_eq!(conversations.latest(&ada()), Some(parent));
}

#[tokio::test]
async fn reading_a_conversation_does_not_make_it_the_principals_most_recent_one() {
    let conversations = Conversations::new();
    assert_eq!(
        conversations.latest(&ada()),
        None,
        "a principal no turn has been served for has no most-recent \
         conversation, which is an answer rather than a default"
    );

    conversations.bind(&ada(), "acme/ada/main").await;
    assert_eq!(
        conversations.resolve("acme/ada/other").await.unwrap(),
        None,
        "and a read of a key nothing has bound is a read all the same"
    );
    assert_eq!(
        conversations.latest(&ada()).unwrap().as_str(),
        "acme/ada/main",
        "a `status` call naming a conversation must not become the answer \
         the next `status` call gets for omitting one"
    );

    // The control: a turn on the other conversation does move it.
    conversations.bind(&ada(), "acme/ada/other").await;
    assert_eq!(
        conversations.latest(&ada()).unwrap().as_str(),
        "acme/ada/other"
    );
    // And one principal's turns are not another's.
    assert_eq!(conversations.latest(&bob()), None);
}

/// R-M9 (M12.1 review, F2): a thread is in the session its own latest turn
/// decided, and the thread's family sharing one cache key does not change
/// that.
///
/// The topology is the oracle's: parent and subagent send one
/// `prompt_cache_key` and two `thread_id`s, so the cache key forks under
/// them while each thread stays pinned to the fork its own turn produced.
/// Here rather than in the shared contract because what it exercises is the
/// *`#g{n}` naming* interacting with the thread map — a fact about this
/// type, not about a correlation backend.
#[tokio::test]
async fn a_thread_is_in_the_session_its_own_latest_turn_decided() {
    let conversations = Conversations::new();
    let key = "acme/ada/main";

    let parent_g0 = conversations.bind(&ada(), key).await;
    conversations
        .bind_thread(&ada(), "thread-parent", parent_g0.clone())
        .await;
    let child_g1 = conversations.fork(&ada(), key).await;
    conversations
        .bind_thread(&ada(), "thread-child", child_g1.clone())
        .await;
    let parent_g2 = conversations.fork(&ada(), key).await;
    conversations
        .bind_thread(&ada(), "thread-parent", parent_g2.clone())
        .await;

    assert_eq!(
        conversations
            .session_of_thread(&ada(), "thread-child")
            .await
            .unwrap(),
        Some(child_g1),
        "the subagent's thread stays in the fork its own turn produced, \
         however far the shared cache key has moved since — this is the \
         whole of F2"
    );
    assert_eq!(
        conversations
            .session_of_thread(&ada(), "thread-parent")
            .await
            .unwrap(),
        Some(parent_g2.clone()),
        "and a thread that forked is in the session it forked *to*: the \
         latest binding wins, where a colliding call id is refused"
    );

    // Binding a thread is not a claim about who is working where: the
    // ingest has already moved `latest` for the turn it is serving.
    assert_eq!(conversations.latest(&ada()), Some(parent_g2));
}

// -----------------------------------------------------------------------
// M14.1: the memo, and the line it is not allowed to cross
// -----------------------------------------------------------------------

/// **The read-through cost R-C2 budgets, counted.** One store read per key
/// per node, and then none.
///
/// Counted at the seam rather than at Redis, because what this file
/// decides is how many times it *asks*: the round trip one ask costs is
/// pinned against a real server by `one_generation_read_is_one_round_trip`
/// in `roundhouse-store-redis`, and the two together are the whole claim.
///
/// The second half — that a commit primes the memo rather than merely
/// invalidating it — is what keeps the *first* turn of a conversation to
/// one read as well: probe, commit, and every later turn is local.
#[tokio::test]
async fn a_key_is_read_through_once_per_node_and_then_memoised() {
    let maps = Arc::new(Double::counting());
    let conversations = Conversations::over(Arc::clone(&maps) as Arc<dyn CorrelationMaps>);
    let key = "acme/ada/main";

    assert_eq!(conversations.generation(key).await, 0);
    assert_eq!(
        maps.reads(),
        1,
        "the node's first touch of a key must go to the store, or a client \
         that reconnected elsewhere silently loses its generation"
    );

    for _ in 0..5 {
        assert_eq!(conversations.generation(key).await, 0);
    }
    assert_eq!(
        maps.reads(),
        1,
        "an absent generation is an answer and must be memoised as one; \
         re-asking the store on every turn is the round trip the \
         write-through cache exists to remove"
    );

    conversations.commit(&ada(), key, 3).await;
    assert_eq!(conversations.generation(key).await, 3);
    assert_eq!(
        maps.reads(),
        1,
        "a commit primes the memo with what this node just wrote, so the \
         next turn of the same conversation reads nothing"
    );

    // CONTROL: the memo is per key, not one slot. A second conversation on
    // the same node still pays its own first read.
    assert_eq!(conversations.generation("acme/ada/other").await, 0);
    assert_eq!(maps.reads(), 2);
}

/// **The line the memo may not cross.** A reader is answered from the
/// store, whatever this node last committed.
///
/// The topology is the one the durable maps exist for: this node committed
/// generation 0 and another node then forked the same key to 1. A `resolve`
/// answered from the memo would narrow the routing of the session the
/// client has just left — F9's defect one fork later, and with a 200 on it.
#[tokio::test]
async fn the_node_memo_does_not_answer_a_reader() {
    let (node_a, node_b) = two_nodes();
    let key = "acme/ada/main";

    let served_here = node_a.bind(&ada(), key).await;
    let forked_elsewhere = node_b.fork(&ada(), key).await;
    assert_ne!(served_here, forked_elsewhere, "sanity: the fork moved");

    assert_eq!(
        node_a.resolve(key).await.unwrap(),
        Some(forked_elsewhere),
        "a reader must answer from the store: this node's memo says \
         generation 0, and the conversation is at 1"
    );

    // CONTROL: the turn path *is* allowed to start from the stale memo,
    // because prefix admission checks whatever it starts from against the
    // log before committing to it. If this ever changes, the read-through
    // cost test above is measuring something else.
    assert_eq!(node_a.generation(key).await, 0);
}

/// The same line for the two binding families: a call another node made
/// ambiguous, and a thread another node moved.
///
/// Both are read from the store on every ask for the same reason `resolve`
/// is — nothing downstream re-checks them — and both would be wrong under
/// a node-local table read first. The call half is M12's F14 with a
/// network in the middle; the thread half is F2's "the session its own
/// latest turn decided", where the latest turn was served elsewhere.
#[tokio::test]
async fn a_binding_another_node_moved_is_read_from_the_store() {
    let (node_a, node_b) = two_nodes();
    let first = SessionId::new("acme/ada/first");
    let second = SessionId::new("acme/ada/second");

    node_a.bind_call(&ada(), "call_0", first.clone()).await;
    node_b.bind_call(&ada(), "call_0", second.clone()).await;
    assert_eq!(
        node_a.session_of_call(&ada(), "call_0").await.unwrap(),
        None,
        "the node that bound the id first must see the collision the \
         second node's claim made, or it answers its own still-open \
         tools/call confidently about the wrong session"
    );

    node_a.bind_thread(&ada(), "thread-1", first).await;
    node_b.bind_thread(&ada(), "thread-1", second.clone()).await;
    assert_eq!(
        node_a.session_of_thread(&ada(), "thread-1").await.unwrap(),
        Some(second),
        "a thread is in the session its own latest turn decided, and that \
         turn was served on the other node"
    );
}

/// **A hint that ran a search off its bound is refreshed** (review M14.1,
/// F2; R-C2″), which is the whole of what [`Conversations::generation`]'s
/// memo may not decide on its own.
///
/// The topology is the durable deployment's: this node's memo is where it
/// left the key, another node has moved it since, and the gap here is
/// wider than [`prefix_admission`](crate::prefix_admission)'s probe bound
/// — which is exactly when a walk from the memo cannot reach the answer.
/// What the search does with the fresh hint is
/// `tests/review_m14_1_f2.rs`'s; what this pins is that asking gets a
/// different answer than the memo, and re-primes it.
#[tokio::test]
async fn a_refresh_replaces_a_hint_the_store_has_moved_past() {
    let (node_a, node_b) = two_nodes();
    let key = "acme/ada/main";

    node_a.commit(&ada(), key, 0).await;
    node_b.commit(&ada(), key, 9).await;
    assert_eq!(
        node_a.generation(key).await,
        0,
        "sanity: the memo is where this node left the key, nine \
         generations behind the store"
    );

    assert_eq!(node_a.generation_refreshed(key).await, 9);
    assert_eq!(
        node_a.generation(key).await,
        9,
        "the refresh re-primes the memo, so the retry behind a refused \
         turn starts where the store is rather than paying the read again"
    );
}

/// **F7 without a Redis: the node that committed agrees with itself, and
/// the write it lost is retried rather than carried for ever.**
///
/// `tests/review_m14_1_f7.rs` drives the same claim through the MCP
/// control surface against a live server whose writes are refused with
/// OOM; this is the seam-level half, and it is what covers the retry
/// through [`Conversations::generation_refreshed`] — the path a *refused*
/// turn takes, where a commit is not coming to do the retry for it.
#[tokio::test]
async fn a_lost_write_is_served_from_the_memo_then_retried_and_cleared() {
    let maps = Arc::new(HalfDownMaps::new());
    let node = Conversations::over(Arc::clone(&maps) as Arc<dyn CorrelationMaps>);
    let elsewhere = Conversations::over(Arc::clone(&maps) as Arc<dyn CorrelationMaps>);
    let key = "acme/ada/main";

    node.commit(&ada(), key, 1).await;
    maps.refuse_writes(true);
    node.commit(&ada(), key, 2).await;
    maps.refuse_writes(false);

    assert_eq!(
        node.resolve(key).await.unwrap(),
        Some(bound_session(key, 2)),
        "this node's own reader must answer the generation this node just \
         committed, not the one the client was moved off"
    );
    assert_eq!(
        elsewhere.resolve(key).await.unwrap(),
        Some(bound_session(key, 1)),
        "sanity: the store really did not take the write, which is what \
         makes the assertion above about the memo and not about the store"
    );

    assert_eq!(
        node.generation_refreshed(key).await,
        2,
        "a refresh retries the lost write instead of reading over it: the \
         store's answer here is the superseded generation, and taking it \
         would hand the next probe a hint that walks backwards"
    );
    assert_eq!(
        elsewhere.resolve(key).await.unwrap(),
        Some(bound_session(key, 2)),
        "and the retry landed, so the walk the lost write cost another \
         node ends here rather than at the next commit"
    );

    // CONTROL: cleared, not pinned. Another node moves the key and this
    // node's reader follows the store to it — which a memo entry still
    // claiming to be dirty would refuse to do, trading one stale answer
    // for another.
    elsewhere.commit(&ada(), key, 3).await;
    assert_eq!(
        node.resolve(key).await.unwrap(),
        Some(bound_session(key, 3))
    );
}

/// A store that cannot be reached is a fact about the deployment, and the
/// two halves of this type treat it differently on purpose.
///
/// The turn path degrades — a generation is a hint, and a hint nobody can
/// load still leaves the probe a starting point — while every reader
/// returns the error, because a reader's `None` reads as "no conversation
/// of yours" and sends the caller to `latest`: a plausible answer about
/// the wrong conversation.
#[tokio::test]
async fn an_unreachable_store_degrades_the_turn_path_and_refuses_the_readers() {
    let conversations = Conversations::over(Arc::new(Double::outage()));
    let key = "acme/ada/main";

    assert_eq!(
        conversations.generation(key).await,
        0,
        "the search still has a place to start"
    );
    // And the failure is not memoised: the next turn asks again rather
    // than serving one outage for the life of the process.
    assert!(conversations.memoised(key).is_none());
    assert!(
        conversations.resolve(key).await.is_err(),
        "a reader asking about a key this node has committed nothing for \
         has only the store to ask, and an unreachable store is not an \
         answer about the caller's tenancy"
    );

    // A commit still moves `latest` and still names the session, so the
    // turn it belongs to is served.
    assert_eq!(
        conversations.commit(&ada(), key, 2).await.as_str(),
        "acme/ada/main#g2"
    );

    // What the same reader gets *after* that commit is F7's ruling and not
    // an exception to the one above: this node committed generation 2, the
    // store refused the write, and the entry that records the refusal is
    // the only place that generation exists. Refusing here would refuse a
    // question this node can answer exactly, about a conversation it is
    // serving right now.
    assert_eq!(
        conversations.resolve(key).await.unwrap(),
        Some(bound_session(key, 2)),
        "the node that committed must agree with itself"
    );
    assert!(
        conversations.resolve("acme/ada/untouched").await.is_err(),
        "and the outage is still an outage for every key this node did \
         not commit through it"
    );
    assert!(
        conversations
            .session_of_call(&ada(), "toolu_1")
            .await
            .is_err()
    );
    assert!(
        conversations
            .session_of_thread(&ada(), "thread-1")
            .await
            .is_err()
    );
}

/// A `CorrelationMaps` double whose one axis is whether it counts reads or
/// fails outright — `CountingMaps` and `OutageMaps` were the same forty-line
/// trait impl twice, differing only in that one dimension (M14.1 review,
/// F3).
///
/// Wrapping the memory maps rather than reimplementing them: what is under
/// test is how often, or whether, `Conversations` reaches the store, and a
/// double with its own semantics would let the count or the failure be right
/// while the answers underneath it were not.
struct Double {
    inner: MemoryCorrelationMaps,
    reads: AtomicUsize,
    outage: bool,
}

impl Double {
    /// Counts every `generation` read and otherwise answers like the real
    /// maps.
    fn counting() -> Self {
        Self {
            inner: MemoryCorrelationMaps::new(),
            reads: AtomicUsize::new(0),
            outage: false,
        }
    }

    /// Never reachable, which is the one failure the trait has.
    fn outage() -> Self {
        Self {
            inner: MemoryCorrelationMaps::new(),
            reads: AtomicUsize::new(0),
            outage: true,
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

fn outage() -> CorrelationError {
    CorrelationError::Backend(anyhow::anyhow!("the correlation store is unreachable"))
}

#[async_trait]
impl CorrelationMaps for Double {
    async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError> {
        if self.outage {
            return Err(outage());
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        CorrelationMaps::generation(&self.inner, key).await
    }

    async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError> {
        if self.outage {
            return Err(outage());
        }
        CorrelationMaps::set_generation(&self.inner, key, generation).await
    }

    async fn bind_call(
        &self,
        principal: &Principal,
        call_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        if self.outage {
            return Err(outage());
        }
        CorrelationMaps::bind_call(&self.inner, principal, call_id, session).await
    }

    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        if self.outage {
            return Err(outage());
        }
        CorrelationMaps::session_of_call(&self.inner, principal, call_id).await
    }

    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        if self.outage {
            return Err(outage());
        }
        CorrelationMaps::bind_thread(&self.inner, principal, thread_id, session).await
    }

    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        if self.outage {
            return Err(outage());
        }
        CorrelationMaps::session_of_thread(&self.inner, principal, thread_id).await
    }
}

/// Maps whose generation *writes* can be turned off while reads keep
/// answering — the partial failure F7 is about, spelled without a Redis.
///
/// A real deployment gets here through one connection of several: the maps
/// hold their own `ConnectionManager`, so a reconnect, a response timeout
/// or an OOM on that one connection loses the write while the session log
/// on its own connection appends. `tests/review_m14_1_f7.rs` drives the
/// same window against a live server with `maxmemory`; this is the half
/// that runs in the ordinary suite.
struct HalfDownMaps {
    inner: MemoryCorrelationMaps,
    writes_refused: AtomicBool,
}

impl HalfDownMaps {
    fn new() -> Self {
        Self {
            inner: MemoryCorrelationMaps::new(),
            writes_refused: AtomicBool::new(false),
        }
    }

    fn refuse_writes(&self, refused: bool) {
        self.writes_refused.store(refused, Ordering::Relaxed);
    }
}

#[async_trait]
impl CorrelationMaps for HalfDownMaps {
    async fn generation(&self, key: &str) -> Result<Option<u32>, CorrelationError> {
        CorrelationMaps::generation(&self.inner, key).await
    }

    async fn set_generation(&self, key: &str, generation: u32) -> Result<(), CorrelationError> {
        if self.writes_refused.load(Ordering::Relaxed) {
            return Err(CorrelationError::Backend(anyhow::anyhow!(
                "this node's connection to the maps lost the write"
            )));
        }
        CorrelationMaps::set_generation(&self.inner, key, generation).await
    }

    async fn bind_call(
        &self,
        principal: &Principal,
        call_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        CorrelationMaps::bind_call(&self.inner, principal, call_id, session).await
    }

    async fn session_of_call(
        &self,
        principal: &Principal,
        call_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        CorrelationMaps::session_of_call(&self.inner, principal, call_id).await
    }

    async fn bind_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
        session: &SessionId,
    ) -> Result<(), CorrelationError> {
        CorrelationMaps::bind_thread(&self.inner, principal, thread_id, session).await
    }

    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, CorrelationError> {
        CorrelationMaps::session_of_thread(&self.inner, principal, thread_id).await
    }
}
