// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! [`mcp_api`](super)'s unit tests, in their own file for the reason the
//! crate's other large modules (`claude_launch`, `relay_handoff`,
//! `control_config::directory`, `control_config::config`) already are:
//! fixtures and ten async tests belong beside the surface they exercise, not
//! inline in the file that keeps growing to hold them (M12.1 review, F3).

use roundhouse_mcp::surface::Correlators;

use super::*;
use crate::ControlPlaneConfig;
use crate::control_config::MembershipError;
use crate::test_support::{bind_conversation, fork_conversation};
use roundhouse_core::control::MemorySpendLedger;
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_core::store::{Lease, MemoryStore};

/// A one-project plane whose `keys` array is whatever the caller writes.
fn plane_with_keys(keys: serde_json::Value) -> ControlPlane {
    plane_with(serde_json::json!({ "min_quality": 0.1 }), keys)
}

/// The same, with the project's policy the caller's to write too.
fn plane_with(policy: serde_json::Value, keys: serde_json::Value) -> ControlPlane {
    let json = serde_json::json!({
        "projects": [{ "id": "acme", "policy": policy }],
        "users": [{ "id": "ada" }],
        "keys": keys,
    })
    .to_string();
    ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "membership fixture")
            .expect("the fixture config must validate"),
    )
}

fn hash(seed: char) -> String {
    seed.to_string().repeat(64)
}

#[test]
fn two_keys_that_mean_different_things_leave_a_membership_with_no_answer() {
    // The probe. Both keys name `acme/ada`; one narrows the project's floor
    // and the other does not. There is no such thing as "ada's policy", so
    // the surface must refuse rather than tell an agent about whichever key
    // the hash map happened to yield first.
    let plane = plane_with_keys(serde_json::json!([
        { "project": "acme", "user": "ada", "key_sha256": hash('a') },
        {
            "project": "acme", "user": "ada", "key_sha256": hash('b'),
            "overrides": { "min_quality": 0.9 }
        },
    ]));
    let ada = Principal::new("acme", "ada");
    assert_eq!(
        plane.membership(&ada).err(),
        Some(MembershipError::Ambiguous(ada.clone()))
    );
    assert_eq!(plane.ambiguous_memberships(), vec![ada.clone()]);
    let refusal =
        describe_ambiguous_memberships(&plane).expect("a startup refusal names the membership");
    assert!(
        refusal.contains("`acme/ada`"),
        "the refusal has to name the membership an operator would go and \
         fix: {refusal}"
    );

    // The control: two keys that mean the *same* thing are a rotation, not
    // an ambiguity, and refusing them would make rotating a secret an
    // outage.
    let rotating = plane_with_keys(serde_json::json!([
        { "project": "acme", "user": "ada", "key_sha256": hash('a') },
        { "project": "acme", "user": "ada", "key_sha256": hash('b') },
    ]));
    assert_eq!(
        rotating
            .membership(&ada)
            .expect("agreeing keys resolve")
            .principal,
        ada
    );
    assert!(rotating.ambiguous_memberships().is_empty());
    assert!(describe_ambiguous_memberships(&rotating).is_none());
}

#[test]
fn two_keys_that_restate_one_policy_are_a_rotation_and_not_an_ambiguity() {
    // The operator move this has to survive: rotating a secret, and copying
    // the policy that is already in force onto the new key so the new row
    // says out loud what it may do. Key A inherits `["local/*"]` from the
    // project; key B restates it as its own override, which *intersects*
    // with the project's — two identical layers where A has one. The two
    // admit exactly the same targets and always will.
    //
    // A digest is a fingerprint of how a policy was written, which is what
    // makes it the right thing to stamp on a `DecisionRecord` and the wrong
    // thing to compare two keys by. Comparing spellings turns this rotation
    // into a boot failure whose message — "different policies or budgets" —
    // is not merely unhelpful but untrue.
    let rotating = plane_with(
        serde_json::json!({ "allow": ["local/*"] }),
        serde_json::json!([
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
            {
                "project": "acme", "user": "ada", "key_sha256": hash('b'),
                "overrides": { "allow": ["local/*"] }
            },
        ]),
    );
    let ada = Principal::new("acme", "ada");

    // The premise, checked rather than assumed: the two really do admit the
    // same set, so what follows is about the comparison and not about the
    // fixture.
    let admissions: Vec<_> = rotating.configured_admissions().collect();
    assert_eq!(admissions.len(), 2);
    for candidate in [
        Candidate {
            target: Target::Local {
                worker_id: 1,
                dp_rank: 0,
                model: "llama".into(),
            },
            expected_prefill_tokens: 0.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 0.0,
            expected_cost_usd: 0.0,
            quality_prior: 0.6,
            load: None,
        },
        Candidate {
            target: Target::Frontier {
                provider: "anthropic".into(),
                model: "claude".into(),
            },
            expected_prefill_tokens: 0.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 0.0,
            expected_cost_usd: 0.0,
            quality_prior: 0.95,
            load: None,
        },
    ] {
        assert_eq!(
            admissions[0].policy.permits(&candidate),
            admissions[1].policy.permits(&candidate),
            "the fixture is only about spelling if both keys agree on \
             `{:?}`",
            candidate.target
        );
    }

    assert_eq!(
        rotating
            .membership(&ada)
            .expect("two spellings of one policy resolve")
            .principal,
        ada
    );
    assert!(rotating.ambiguous_memberships().is_empty());
    assert!(describe_ambiguous_memberships(&rotating).is_none());

    // The same shape one step further out: a project that narrows nothing
    // and a key whose override says `*` out loud. Both admit everything.
    let spelled_out = plane_with(
        serde_json::json!({}),
        serde_json::json!([
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
            {
                "project": "acme", "user": "ada", "key_sha256": hash('b'),
                "overrides": { "allow": ["*"] }
            },
        ]),
    );
    assert!(
        spelled_out.membership(&ada).is_ok(),
        "a layer that names `*` admits every target, so it constrains \
         nothing and cannot make two keys disagree"
    );

    // The control, and the whole reason the check exists: two keys that
    // really do mean different things still have no resolvable membership.
    // Without this the assertions above would pass for a comparison that
    // had simply been deleted.
    let disagreeing = plane_with(
        serde_json::json!({ "allow": ["local/*"] }),
        serde_json::json!([
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
            {
                "project": "acme", "user": "ada", "key_sha256": hash('b'),
                "overrides": { "min_quality": 0.9 }
            },
        ]),
    );
    assert_eq!(
        disagreeing.membership(&ada).err(),
        Some(MembershipError::Ambiguous(ada.clone()))
    );
}

/// A store that is up enough to be called and down enough to answer
/// nothing.
///
/// [`MemoryStore`] can only ever produce [`StoreError::SessionNotFound`],
/// which is why no existing test could tell a store outage apart from a
/// tenancy verdict: the one arm that renders as "not yours" was also the
/// only arm reachable. `RedisSessionStore::last_seq` returns
/// [`StoreError::Backend`] for a transport failure *and* for its
/// foreign-writer contiguity check, so this is the shape a real deployment
/// hits on a connection reset, not an invented one.
struct OutageStore;

#[async_trait]
impl SessionStore for OutageStore {
    async fn create_session(&self, _: &SessionId, _: &str) -> Result<bool, StoreError> {
        unreachable!("the control surface never writes to a session log")
    }
    async fn acquire_lease(
        &self,
        _: &SessionId,
        _: &str,
        _: u64,
    ) -> Result<Option<Lease>, StoreError> {
        unreachable!("the control surface takes no lease -- see this module's doc")
    }
    async fn renew_lease(&self, _: &Lease, _: u64) -> Result<Option<Lease>, StoreError> {
        unreachable!("the control surface takes no lease -- see this module's doc")
    }
    async fn release_lease(&self, _: &Lease) -> Result<(), StoreError> {
        unreachable!("the control surface takes no lease -- see this module's doc")
    }
    async fn append_events(
        &self,
        _: &Lease,
        _: Vec<SessionEventKind>,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        unreachable!("the control surface never writes to a session log")
    }
    async fn read_events(
        &self,
        _: &SessionId,
        _: u64,
        _: usize,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        Err(StoreError::Backend(anyhow::anyhow!(
            "redis connection reset"
        )))
    }
    async fn last_seq(&self, _: &SessionId) -> Result<u64, StoreError> {
        Err(StoreError::Backend(anyhow::anyhow!(
            "redis connection reset"
        )))
    }
}

fn reads_over<S: SessionStore>(plane: ControlPlane, store: Arc<S>) -> ControlPlaneReads<S> {
    reads_sharing(plane, store, Arc::new(Conversations::new()))
}

/// As [`reads_over`], but over a conversation table the test also holds.
///
/// The composition root shares one table between the turn surfaces and this
/// one; a test asserting how a tool-use id resolves has to write the binding
/// from the same side a turn would, or it is asserting against a table
/// nothing populates.
/// No `_meta` at all — a client that attaches neither correlator.
fn uncorrelated() -> Correlators {
    Correlators::default()
}

/// A Claude Code call: `_meta["claudecode/toolUseId"]` and nothing else.
fn answering(tool_use_id: &str) -> Correlators {
    Correlators {
        tool_use_id: Some(tool_use_id.to_string()),
        ..Correlators::default()
    }
}

/// A Codex call: `_meta.threadId` and nothing else.
fn in_thread(thread_id: &str) -> Correlators {
    Correlators {
        thread_id: Some(thread_id.to_string()),
        ..Correlators::default()
    }
}

fn reads_sharing<S: SessionStore>(
    plane: ControlPlane,
    store: Arc<S>,
    conversations: Arc<Conversations>,
) -> ControlPlaneReads<S> {
    ControlPlaneReads::new(
        Arc::new(plane),
        store,
        Arc::new(MemorySpendLedger::new()),
        conversations,
        Vec::new(),
    )
}

/// A plane that authenticates two tenants, for the tenancy half of R-M2.
///
/// Spelled out rather than reached through [`plane_with_keys`], which
/// declares one project and one user by construction — a second tenant is
/// exactly what this half needs and exactly what that helper cannot express.
fn two_tenant_plane() -> ControlPlane {
    let json = serde_json::json!({
        "projects": [
            { "id": "acme", "policy": { "min_quality": 0.1 } },
            { "id": "globex", "policy": { "min_quality": 0.1 } },
        ],
        "users": [{ "id": "ada" }, { "id": "bob" }],
        "keys": [
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
            { "project": "globex", "user": "bob", "key_sha256": hash('b') },
        ],
    })
    .to_string();
    ControlPlane::configured(
        ControlPlaneConfig::from_json(&json, "two-tenant fixture")
            .expect("the fixture config must validate"),
    )
}

/// R-M2 (M12): a tool-use id resolves the conversation that emitted it, and
/// it beats the `latest` guess.
///
/// **The race this removes, made deterministic.** One principal drives two
/// conversations — a parent agent and the subagent it spawned. Both bind
/// through the same table, so `latest` names whichever opened a turn most
/// recently, and an MCP call from the *other* one reads and narrows the
/// wrong session. Here the parent's conversation is bound second, so
/// `latest` is the parent's and the subagent's id is the only thing that
/// can point at the subagent's log.
#[tokio::test]
async fn a_tool_use_id_resolves_the_conversation_that_emitted_it() {
    let ada = Principal::new("acme", "ada");
    let conversations = Arc::new(Conversations::new());
    let subagent = bind_conversation(&conversations, &ada, "acme/ada/sub").await;
    let parent = bind_conversation(&conversations, &ada, "acme/ada/main").await;
    conversations
        .bind_call(&ada, "toolu_sub", subagent.clone())
        .await;

    let reads = reads_sharing(
        two_tenant_plane(),
        Arc::new(MemoryStore::new()),
        Arc::clone(&conversations),
    );

    assert_eq!(
        reads
            .resolve_session(&ada, None, &answering("toolu_sub"))
            .await
            .expect("an id this node emitted for this caller resolves"),
        subagent,
        "the call came from the subagent's tool loop; `latest` would have              answered with the parent's conversation"
    );

    // The control that proves the assertion above is about the id and not
    // about the ordering: with no id, the guess is what is left.
    assert_eq!(
        reads
            .resolve_session(&ada, None, &uncorrelated())
            .await
            .expect("a principal with a most recent conversation"),
        parent,
        "Codex sends no such key, and the surface must answer it exactly              as it did before R-M2"
    );

    // And an argument the *model* wrote is served when the id the *client*
    // attached agrees with it.
    //
    // **This assertion used to read "the argument outranks the id"**, which
    // is how R-M2 left it. R-M7 narrowed that: a disagreement is now a
    // refusal rather than a precedence question — see
    // `an_argument_and_a_correlator_naming_two_conversations_are_refused`
    // below — so what survives here is the agreeing case.
    let store = Arc::new(MemoryStore::new());
    store
        .create_session(&parent, "gpt-4")
        .await
        .expect("a session for the named path to find");
    conversations
        .bind_call(&ada, "toolu_parent", parent.clone())
        .await;
    let named = reads_sharing(two_tenant_plane(), store, Arc::clone(&conversations));
    assert_eq!(
        named
            .resolve_session(&ada, Some("main"), &answering("toolu_parent"))
            .await
            .expect("the named conversation exists"),
        parent,
    );
}

/// R-M7: `_meta.threadId` is the conversation the client names, resolved
/// through the same `qualify` the `conversation` argument goes through.
///
/// **The race, made deterministic**, exactly as the tool-use id test above
/// makes it: the parent binds second, so `latest` is the parent's, and an
/// answer naming the subagent's log can only have come from the thread id.
/// A codex client stamps this on every `tools/call` and the value is that
/// turn's `prompt_cache_key` — which is what the fixture's `"sub"` stands
/// for here.
#[tokio::test]
async fn a_thread_id_resolves_the_conversation_the_client_names() {
    let ada = Principal::new("acme", "ada");
    let conversations = Arc::new(Conversations::new());
    let subagent = bind_conversation(&conversations, &ada, "acme/ada/sub").await;
    let parent = bind_conversation(&conversations, &ada, "acme/ada/main").await;

    // Both conversations real in the store, because the thread id goes down
    // the *named* path and that path checks existence. A rig whose rival
    // existed only in the binding table would make the fall-through below
    // fail loudly instead of quietly, which is the weaker claim.
    let store = Arc::new(MemoryStore::new());
    for session in [&subagent, &parent] {
        store
            .create_session(session, "gpt-4")
            .await
            .expect("a conversation the named path can find");
    }
    let reads = reads_sharing(two_tenant_plane(), store, Arc::clone(&conversations));

    assert_eq!(
        reads
            .resolve_session(&ada, None, &in_thread("sub"))
            .await
            .expect("the thread the client named is this caller's"),
        subagent,
        "the call came from the subagent's thread; `latest` would have \
         answered with the parent's conversation"
    );

    // The control: with no correlator the guess stands, and it is the other
    // conversation — which is what makes the assertion above about the
    // thread id rather than about there being one answer available.
    assert_eq!(
        reads
            .resolve_session(&ada, None, &uncorrelated())
            .await
            .expect("a principal with a most recent conversation"),
        parent,
    );

    // A thread id naming nothing of this caller's falls through to the
    // guess rather than refusing — the same collapse an unknown tool-use id
    // gets, and for the same enumeration-oracle reason. The *argument* with
    // the same string refuses instead, which is the asymmetry R-M7 rules.
    assert_eq!(
        reads
            .resolve_session(&ada, None, &in_thread("no-such-thread"))
            .await
            .expect("an unknown correlator is not a refusal"),
        parent,
    );
    assert!(
        matches!(
            reads
                .resolve_session(&ada, Some("no-such-thread"), &uncorrelated())
                .await,
            Err(SurfaceError::ForeignConversation(_))
        ),
        "a name the model wrote is a question about that name; a \
         correlator is context the client volunteered"
    );
}

/// R-M7's tenancy half for the thread id: another tenant's cache key names
/// nothing here, and it never reaches that tenant's session.
#[tokio::test]
async fn another_tenants_thread_id_never_resolves_to_that_tenants_session() {
    let ada = Principal::new("acme", "ada");
    let bob = Principal::new("globex", "bob");
    let conversations = Arc::new(Conversations::new());
    let adas = bind_conversation(&conversations, &ada, "acme/ada/main").await;

    let store = Arc::new(MemoryStore::new());
    store
        .create_session(&adas, "gpt-4")
        .await
        .expect("ada's conversation");
    let reads = reads_sharing(two_tenant_plane(), store, Arc::clone(&conversations));

    // `main` is ada's cache key. Qualified into bob's namespace it names
    // nothing — which is the whole of the check, and why reading a
    // client-supplied name needs no second tenancy rule.
    let refused = reads
        .resolve_session(&bob, None, &in_thread("main"))
        .await
        .expect_err("bob has no conversation of his own to fall back to");
    assert!(
        matches!(refused, SurfaceError::NoSession),
        "a thread id belonging to another tenant is a thread id this \
         deployment has none of for *this* caller: {refused}"
    );

    let bobs = bind_conversation(&conversations, &bob, "globex/bob/main").await;
    assert_eq!(
        reads
            .resolve_session(&bob, None, &in_thread("main"))
            .await
            .expect("bob's own most recent conversation"),
        bobs,
        "and once bob has one he gets his own, never ada's, with no hint \
         that the name he presented meant anything to anybody"
    );
    assert_ne!(bobs, adas);
}

/// F2 (M12.1 review, correlation-boundary) control: a subagent's *own*
/// `_meta.threadId`, as the pinned codex oracle (`6344a65`) actually sends
/// it, was never bound as any session key.
///
/// Established independently of [`Conversations`]'s internals, through the
/// same `conversation`-argument path a model-written name goes through: a
/// string this deployment has never seen refuses as
/// [`SurfaceError::ForeignConversation`] rather than resolving to
/// anything. This is the premise the next test's failure rests on — proof
/// that the string in question really is unbound, not a typo in the
/// fixture.
///
/// **Why the string is unbound in the first place, per the oracle.**
/// `client.rs::prompt_cache_key` sends `responses_metadata.session_id`,
/// and `agent/control.rs:104-110`'s own doc comment says that id "is
/// shared by the whole agent control session... every sub-agents from a
/// common root share the same session ID" — so every turn any member of
/// the family drives, parent or subagent, is bound under *one* cache key.
/// `session/session.rs:671-676` sets a non-root agent's `session_id` from
/// its *own* thread id only for a legacy resumed rollout; live, it takes
/// `agent_control.session_id()` — the shared one. But
/// `mcp_tool_call.rs`'s `with_mcp_tool_call_thread_id_meta` stamps
/// `sess.thread_id` (`session/turn_context.rs:618-620`'s
/// `TurnMetadataState`, a distinct id per session/subagent) as
/// `_meta.threadId` — never the shared `session_id`. The M12.1 doc
/// comment's premise — "captured traffic shows it byte-identical to the
/// turn's `prompt_cache_key`" (`reads.rs`, R-M7 rule 2) — holds only for a
/// codex root thread, which is the whole of F2.
#[tokio::test]
async fn f2_control_a_subagents_own_thread_id_was_never_bound_as_a_cache_key() {
    let ada = Principal::new("acme", "ada");
    let conversations = Arc::new(Conversations::new());
    // The shared key every member of the family actually sends as
    // `prompt_cache_key` -- nothing in this test ever binds the
    // subagent's own thread id under any key.
    let _parent = bind_conversation(&conversations, &ada, "acme/ada/main").await;

    let reads = reads_over(two_tenant_plane(), Arc::new(MemoryStore::new()));
    let refused = reads
        .resolve_session(&ada, Some("subagent-own-thread-id"), &uncorrelated())
        .await
        .expect_err(
            "a subagent's own thread id, as the oracle actually sends it, names no \
             conversation of this deployment's -- it was never a `prompt_cache_key`",
        );
    assert!(
        matches!(refused, SurfaceError::ForeignConversation(_)),
        "the string genuinely resolves to nothing as a *name*, which is \
         the premise \
         `f2_a_subagents_thread_id_resolves_its_own_fork_and_not_the_parents` \
         depends on: what answers that test is the thread table the ingest \
         writes, and nothing this one can reach: {refused}"
    );
}

/// F2 (M12.1 review, correlation-boundary), closed by R-M9: a codex
/// subagent's `_meta.threadId` resolves the fork *its own* turn produced,
/// not whichever fork of the family's shared cache key ran last.
///
/// **The topology, built the way the oracle actually produces it rather
/// than the M12.1 fixtures' `threadId == cache_key`.** Parent and subagent
/// share one cache key (see the control test above); a subagent's history
/// is a genuinely different conversation under that one key, so its first
/// turn looks to roundhouse exactly like a client that rewrote its own
/// history — the trigger `Conversations::commit`'s doc names — and forks the
/// key. The parent then drives another turn on the *same* shared key
/// (concurrent multi-agent orchestration, or simply resuming while the
/// subagent's own tool loop is still in flight) and forks it again, moving
/// `latest` out from under the subagent. The subagent's own `tools/call`
/// then arrives stamped with its own thread id.
///
/// **What the fix added, and what this fixture stands in for.** Each
/// `bind_thread` below is the write `responses_api`'s bind/fork function
/// makes when a turn arrives carrying `x-codex-turn-metadata`, whose
/// `thread_id` is that turn's *own* thread
/// (`core/src/responses_metadata.rs:281`, from the per-session
/// `TurnMetadataState` at `core/src/session/turn_context.rs:618-622`) —
/// the one thing on the wire that tells a subagent's turn from its
/// parent's, where `prompt_cache_key` cannot
/// (`core/src/agent/control.rs:104-110`). The end-to-end version, over the
/// real header and the real router, is
/// `a_codex_subagents_thread_id_resolves_its_own_fork` in
/// `tests/mcp_surface.rs`; this one is the resolver-level twin the finding
/// was written against.
///
/// Before R-M9 this failed: `named_session` refused the subagent's thread
/// id as foreign (the control test above), R-M7's thread branch swallowed
/// that to `None` exactly as it swallows an unknown tool-use id, and the
/// call fell through to `latest` — the parent's fork. The second assertion
/// is what keeps that from being re-provable by accident: with no
/// correlator at all the guess still lands on the parent, so the first
/// assertion is about the thread id carrying information and not about
/// there being one answer available.
#[tokio::test]
async fn f2_a_subagents_thread_id_resolves_its_own_fork_and_not_the_parents() {
    let ada = Principal::new("acme", "ada");
    let conversations = Arc::new(Conversations::new());

    // The one shared cache key the whole family sends as
    // `prompt_cache_key`, per `agent/control.rs:104-110`.
    let key = "acme/ada/main";
    let parent_g0 = bind_conversation(&conversations, &ada, key).await;
    conversations
        .bind_thread(&ada, "parent-own-thread-id", parent_g0.clone())
        .await;

    // The subagent's turn: a different conversation under the same key
    // reads, to roundhouse, as a client that rewrote its own history.
    let subagent_fork = fork_conversation(&conversations, &ada, key).await;
    assert_eq!(subagent_fork.as_str(), "acme/ada/main#g1");
    conversations
        .bind_thread(&ada, "subagent-own-thread-id", subagent_fork.clone())
        .await;

    let store = Arc::new(MemoryStore::new());
    for session in [&parent_g0, &subagent_fork] {
        store
            .create_session(session, "gpt-4")
            .await
            .expect("a conversation the named path can find");
    }
    let reads = reads_sharing(
        two_tenant_plane(),
        Arc::clone(&store),
        Arc::clone(&conversations),
    );

    // Mid-subagent-turn, the parent drives another turn on the same
    // shared key, forking again and moving `latest` to the parent.
    let parent_fork2 = fork_conversation(&conversations, &ada, key).await;
    assert_eq!(parent_fork2.as_str(), "acme/ada/main#g2");
    conversations
        .bind_thread(&ada, "parent-own-thread-id", parent_fork2.clone())
        .await;
    store
        .create_session(&parent_fork2, "gpt-4")
        .await
        .expect("a conversation the named path can find");

    // The subagent's own `tools/call`, stamped with the subagent's own
    // thread id -- exactly as the oracle sends it.
    let resolved = reads
        .resolve_session(&ada, None, &in_thread("subagent-own-thread-id"))
        .await
        .expect("the subagent's thread is one this deployment served");

    assert_eq!(
        resolved, subagent_fork,
        "the subagent's tools/call must reach the fork its own turn \
         produced, not the parent's latest one ({parent_fork2:?}) -- the \
         race M12.1 set out to close, for the topology the addendum names"
    );

    // The parent's own call still reaches the parent, so the thread table
    // is telling two threads apart rather than merely preferring the
    // older fork.
    assert_eq!(
        reads
            .resolve_session(&ada, None, &in_thread("parent-own-thread-id"))
            .await
            .expect("the parent's thread is one this deployment served"),
        parent_fork2,
    );

    // The control proving the first assertion is about the thread id
    // carrying information: with no correlator supplied at all, the guess
    // lands on the parent's fork.
    let guessed = reads
        .resolve_session(&ada, None, &uncorrelated())
        .await
        .expect("a principal with a most recent conversation");
    assert_eq!(
        guessed, parent_fork2,
        "`latest` alone cannot tell the family's members apart, which is \
         why the thread id has to"
    );
}

/// R-M7's refusal: the model's argument and the client's correlator naming
/// two different conversations is not a precedence question.
#[tokio::test]
async fn an_argument_and_a_correlator_naming_two_conversations_are_refused() {
    let ada = Principal::new("acme", "ada");
    let conversations = Arc::new(Conversations::new());
    let subagent = bind_conversation(&conversations, &ada, "acme/ada/sub").await;
    let parent = bind_conversation(&conversations, &ada, "acme/ada/main").await;
    conversations
        .bind_call(&ada, "toolu_sub", subagent.clone())
        .await;

    let store = Arc::new(MemoryStore::new());
    for session in [&subagent, &parent] {
        store.create_session(session, "gpt-4").await.expect(
            "both conversations exist, so neither half is refused \
                     by the foreign-name door",
        );
    }
    let reads = reads_sharing(two_tenant_plane(), store, Arc::clone(&conversations));

    for correlators in [answering("toolu_sub"), in_thread("sub")] {
        let refused = reads
            .resolve_session(&ada, Some("main"), &correlators)
            .await
            .expect_err("a caller contradicting itself is refused");
        let message = refused.to_string();
        assert!(
            matches!(refused, SurfaceError::ContradictoryConversation { .. }),
            "and refused as its own variant, not as a verdict about either \
             conversation's tenancy: {message}"
        );
        assert!(
            message.contains(parent.as_str()) && message.contains(subagent.as_str()),
            "the refusal must name both, or the agent cannot tell which of \
             its own two inputs to change: {message}"
        );
    }
}

/// R-M2's tenancy half: another principal's id names nothing here.
///
/// **Refused rather than answered when there is nothing else to answer
/// with**, and answered with the caller's *own* conversation when there is
/// — never with the conversation the id actually belongs to. The two are one
/// rule: an id that is not this caller's is treated exactly as an id this
/// node never emitted, so a probe learns nothing it did not already know.
#[tokio::test]
async fn another_principals_tool_use_id_never_resolves_to_that_principals_session() {
    let ada = Principal::new("acme", "ada");
    let bob = Principal::new("globex", "bob");
    let conversations = Arc::new(Conversations::new());
    let adas = bind_conversation(&conversations, &ada, "acme/ada/main").await;
    conversations
        .bind_call(&ada, "toolu_ada", adas.clone())
        .await;

    let reads = reads_sharing(
        two_tenant_plane(),
        Arc::new(MemoryStore::new()),
        Arc::clone(&conversations),
    );

    let refused = reads
        .resolve_session(&bob, None, &answering("toolu_ada"))
        .await
        .expect_err("bob has no conversation of his own to fall back to");
    assert!(
        matches!(refused, SurfaceError::NoSession),
        "an id belonging to another tenant is an id this deployment has              none of for *this* caller: {refused}"
    );

    // Once bob has one, he gets his own — never ada's, and with no hint
    // that the id he presented meant anything to anybody.
    let bobs = bind_conversation(&conversations, &bob, "globex/bob/main").await;
    assert_eq!(
        reads
            .resolve_session(&bob, None, &answering("toolu_ada"))
            .await
            .expect("bob's own most recent conversation"),
        bobs,
    );
}

#[tokio::test]
async fn a_store_outage_is_reported_as_infrastructure_rather_than_as_tenancy() {
    // The enumeration-oracle argument above justifies collapsing "never
    // existed" and "belongs to another tenant" into one answer. It does not
    // justify collapsing "the store is down" into the same one: an outage is
    // not a per-conversation signal, so answering it as one tells an agent —
    // in its own context, about its own conversation — that the conversation
    // is somebody else's. That is the least actionable answer available,
    // because it invites a re-`init_session` or a give-up where an
    // `Internal` invites a retry.
    let plane = plane_with_keys(serde_json::json!([
        { "project": "acme", "user": "ada", "key_sha256": hash('a') },
    ]));
    // Bound, because since F9 a name reaches the store only for a key this
    // node has bound -- an unbound one is refused before any store is
    // asked, which would make this fixture pass for the wrong reason.
    let conversations = Arc::new(Conversations::new());
    bind_conversation(
        &conversations,
        &Principal::new("acme", "ada"),
        "acme/ada/main",
    )
    .await;
    let reads = reads_sharing(plane, Arc::new(OutageStore), Arc::clone(&conversations));

    let error = reads
        .resolve_session(
            &Principal::new("acme", "ada"),
            Some("main"),
            &uncorrelated(),
        )
        .await
        .expect_err("a store that cannot answer has not answered");
    assert!(
        matches!(error, SurfaceError::Internal(_)),
        "a store outage must reach the agent as an internal error it can \
         retry, not as a verdict about whose conversation this is: {error}"
    );
    assert!(
        error.to_string().contains("redis connection reset"),
        "and it must carry the backend's own diagnosis, or an operator \
         reading the agent's transcript learns nothing: {error}"
    );

    // The control, and the reason the collapse is right for the case it was
    // written for: a store that is up and simply holds no such session still
    // answers with the tenancy verdict, so `SessionNotFound` and "another
    // tenant's" stay indistinguishable from here.
    let closed = reads_sharing(
        plane_with_keys(serde_json::json!([
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
        ])),
        Arc::new(MemoryStore::new()),
        conversations,
    );
    let refused = closed
        .resolve_session(
            &Principal::new("acme", "ada"),
            Some("main"),
            &uncorrelated(),
        )
        .await
        .expect_err("a session nobody created is not this caller's");
    assert!(
        matches!(refused, SurfaceError::ForeignConversation(ref named) if named == "main"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_store_outage_on_the_thread_correlator_is_also_infrastructure_not_tenancy() {
    // M12.1 review F1. The test above drives a store outage through the
    // `conversation` *argument*; nothing before this one drove one through
    // the `_meta.threadId` *correlator*, which has its own hand-written
    // match at the `thread:` arm above (`Err(ForeignConversation) => None`,
    // every other error `return`ed). This is the control proving that arm
    // is spelled correctly here. It says nothing about `FakeDeployment` or
    // `IndependentReads` -- the two `ControlReads` test doubles that stand
    // in for this same trait method spell the swallow as a bare `.ok()`,
    // which would also eat an `Internal`. Neither double can even be asked
    // this question: both resolve names through `FakeDeployment::named_session`,
    // a `HashMap` lookup with no store behind it, so `Internal` is not a
    // value either can produce, let alone be tested against.
    let plane = plane_with_keys(serde_json::json!([
        { "project": "acme", "user": "ada", "key_sha256": hash('a') },
    ]));
    // Bound as a cache key and *not* as a thread, so the correlator takes
    // the name path this test is about: a thread binding would answer from
    // the node's own table and never reach the store at all.
    let conversations = Arc::new(Conversations::new());
    bind_conversation(
        &conversations,
        &Principal::new("acme", "ada"),
        "acme/ada/main",
    )
    .await;
    let reads = reads_sharing(plane, Arc::new(OutageStore), conversations);

    let error = reads
        .resolve_session(&Principal::new("acme", "ada"), None, &in_thread("main"))
        .await
        .expect_err("a store that cannot answer has not answered");
    assert!(
        matches!(error, SurfaceError::Internal(_)),
        "a store outage reached through _meta.threadId must reach the agent \
         as an internal error it can retry, not be swallowed as an unknown \
         correlator falling through to `latest`: {error}"
    );
    assert!(
        error.to_string().contains("redis connection reset"),
        "and it must carry the backend's own diagnosis: {error}"
    );
}

/// A store that counts its `last_seq` calls, so a test can see how many
/// round trips one `resolve_session` call actually spent.
struct CountingStore<S> {
    inner: S,
    last_seq_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl<S: SessionStore> SessionStore for CountingStore<S> {
    async fn create_session(
        &self,
        session_id: &SessionId,
        model_policy: &str,
    ) -> Result<bool, StoreError> {
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
        self.inner.read_events(session_id, after_seq, limit).await
    }
    async fn last_seq(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        self.last_seq_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.last_seq(session_id).await
    }
}

/// A [`PlaneSource`] that counts its `plane()` calls, so a test can see
/// how many independent compiled-plane snapshots one `resolve_session`
/// call actually took.
struct CountingPlaneSource {
    inner: ControlPlane,
    plane_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl PlaneSource for CountingPlaneSource {
    fn plane(&self, _now_ms: u64) -> Arc<ControlPlane> {
        self.plane_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Arc::new(self.inner.clone())
    }
}

#[tokio::test]
async fn f8_an_agreeing_argument_and_correlator_resolve_the_same_string_once() {
    // M12.1 review F8. `conversation` and `_meta.threadId` naming the same
    // string is the ordinary case for a Codex model naming the thread it is
    // already in. `resolve_session` (mcp_api.rs:199-201, 223-228) runs
    // `named_session` once for the argument and once more for the
    // correlator, with no check that the two strings already agree -- so
    // the same name is qualified against the plane and looked up in the
    // store twice for one answer.
    let ada = Principal::new("acme", "ada");
    let plane = plane_with_keys(serde_json::json!([
        { "project": "acme", "user": "ada", "key_sha256": hash('a') },
    ]));

    let plane_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let planes = Arc::new(CountingPlaneSource {
        inner: plane.clone(),
        plane_calls: Arc::clone(&plane_calls),
    });

    // The exact session `named_session` will resolve "main" to, computed
    // through the same table `reads` is given below, so the fixture and
    // the code under test agree on what "main" means.
    let conversations = Arc::new(Conversations::new());
    // Bound rather than merely resolved: since F9 a reader answers only
    // for keys this node has bound, so a fixture that computed the id
    // without binding it would be asking about a conversation this node
    // has never served.
    let session = bind_conversation(&conversations, &ada, &plane.qualify(&ada, "main")).await;

    let last_seq_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = Arc::new(CountingStore {
        inner: MemoryStore::new(),
        last_seq_calls: Arc::clone(&last_seq_calls),
    });
    store
        .create_session(&session, "gpt-4")
        .await
        .expect("a session for the fixture's own conversation to exist");

    let reads = ControlPlaneReads::new(
        planes,
        store,
        Arc::new(MemorySpendLedger::new()),
        conversations,
        Vec::new(),
    );

    let resolved = reads
        .resolve_session(&ada, Some("main"), &in_thread("main"))
        .await
        .expect("an agreeing argument and correlator both name a real conversation");
    assert_eq!(
        resolved, session,
        "the fixture must be exercising the conversation it thinks it is"
    );

    assert_eq!(
        last_seq_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one string, checked against the store once -- HEAD spends a \
         second round trip re-confirming what the argument already \
         established"
    );
    assert_eq!(
        plane_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one string, qualified against one plane snapshot -- HEAD takes a \
         second independent plane() that could, under a concurrent \
         reload, disagree with the first"
    );
}

#[tokio::test]
async fn the_server_reads_arm_answers_the_cursor_so_the_status_memo_can_fire() {
    // The surface memoises `session_facts` behind `session_cursor`, but a
    // memo whose cursor is always `None` never fires — every `status` and
    // `explain_last_route` a model calls replays the whole log. The trait's
    // default answers `None`; this arm must override it over the same
    // `last_seq` `resolve_session` already reads, or the memo is dead on the
    // shipped deployment. Dropping the override regresses this to `None`.
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new("acme/ada/main");
    store
        .create_session(&session, "gpt-4")
        .await
        .expect("a session to read a cursor from");
    let reads = reads_over(
        plane_with_keys(serde_json::json!([
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
        ])),
        store,
    );
    assert_eq!(
        reads.session_cursor(&session).await.expect("a cursor read"),
        Some(0),
        "a created session's cursor is its last seq, not the `None` that \
         leaves the memo inert"
    );

    // And a store that cannot answer becomes `None`, never an error: the
    // cursor is only an optimization, so an outage falls back to
    // project-every-time rather than failing a tool that would have worked.
    let downed = reads_over(
        plane_with_keys(serde_json::json!([
            { "project": "acme", "user": "ada", "key_sha256": hash('a') },
        ])),
        Arc::new(OutageStore),
    );
    assert_eq!(
        downed
            .session_cursor(&SessionId::new("acme/ada/main"))
            .await
            .expect("a cursor read never errors -- it degrades to None"),
        None,
    );
}

#[test]
fn an_unconfigured_deployment_answers_for_every_principal_and_a_configured_one_does_not() {
    // Open mode admits every request as one membership, so asking about it
    // backwards has to give the same value asking forwards does — one
    // definition of what an unconfigured deployment allows.
    let open = ControlPlane::Open;
    let admission = open
        .membership(&Principal::new("anyone", "at-all"))
        .expect("open mode never refuses");
    assert_eq!(admission.principal, Principal::default_open());
    assert_eq!(*admission.policy, TurnPolicy::unrestricted());
    assert!(admission.budget.is_none());

    // A configured deployment refuses a principal no key names, rather than
    // falling back to the unrestricted policy — which is the one answer that
    // would be a privilege escalation rather than an error.
    let plane = plane_with_keys(serde_json::json!([
        { "project": "acme", "user": "ada", "key_sha256": hash('a') },
    ]));
    let stranger = Principal::new("acme", "nobody");
    assert_eq!(
        plane.membership(&stranger).err(),
        Some(MembershipError::Unknown(stranger))
    );
}
