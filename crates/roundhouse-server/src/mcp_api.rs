// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The MCP control surface, mounted as this deployment's fourth router.
//!
//! [`roundhouse_mcp`] holds the tools and their semantics and depends on
//! nothing of ours but `roundhouse-core`. This file is the other half of that
//! split: it implements the one seam the surface reads a deployment through
//! ([`ControlReads`]), it puts the existing bearer-key resolution in front of
//! the route, and it hands both to the composition root. The dependency runs
//! `server -> mcp -> core` and never the other way, which is what keeps the tool
//! contract testable with no engine, no store and no socket in sight.
//!
//! # What a control tool may touch
//!
//! Every method below is a read. The surface's *writes* go to the shared
//! [`ControlStore`], never to a session log — an MCP request arrives on its own
//! HTTP request, and a log has exactly one writer at a time (the turn gate
//! within a process, the store's lease across them). That is why nothing here
//! opens a [`Session`](roundhouse_core::session::Session): a reader that took
//! the lease would evict the engine it is reporting on. It projects instead,
//! through [`SessionState::project`], which is the same fold the engine's own
//! replay runs.
//!
//! # Authentication is the deployment's, not this surface's
//!
//! [`auth_layer`] calls [`ControlPlane::scope`] — the same function the turn
//! surfaces resolve keys through, and the only one — and puts the resolved
//! [`Principal`] into the request's extensions, where the transport reads it
//! back. An admin key is refused here rather than in `scope`, for the reason
//! [`ControlPlane::turn_admission`] refuses one there: whether an admin key is
//! the wrong key depends on what the route wanted, and this route wants a
//! tenant. A request that reaches a tool with no principal in its extensions is
//! refused by the transport rather than served to a default one.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post_service;

use roundhouse_core::control::{Balance, BalanceQuery, Principal, SpendLedger, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::now_ms;
use roundhouse_core::routing::{CacheLedger, Candidate, Target};
use roundhouse_core::session::SessionState;
use roundhouse_core::store::{SessionStore, StoreError};
use roundhouse_mcp::reads::{ControlReads, SessionFacts};
use roundhouse_mcp::surface::SurfaceError;
use roundhouse_mcp::transport::HostGuard;
use roundhouse_mcp::{ControlPlaneSurface, ControlStore};

use crate::control_config::{AuthError, ControlPlane, KeyScope, PlaneSource};
use crate::conversations::Conversations;
use crate::http::ApiError;

/// Everything the control surface reads a running deployment through.
///
/// Assembled at the composition root from the same values the turn surfaces
/// were built with — one control plane, one store, one ledger, one conversation
/// table — because a second copy of any of them would be a control surface that
/// reports on a deployment adjacent to the one serving turns.
pub struct ControlPlaneReads<S: SessionStore> {
    /// A [`PlaneSource`] rather than a compiled plane, for the reason every
    /// other surface holds one: an agent whose key was revoked mid-session must
    /// stop being told what it may route to, and a copy captured at mount time
    /// would answer for the life of the process.
    ///
    /// Re-asked per read rather than once per tool call, because this trait has
    /// no request scope to hang a snapshot on and each method is an independent
    /// question. The two reads inside one tool call can therefore straddle a
    /// write — the answer is then simply the newer of two compiled planes,
    /// which is what a second tool call a millisecond later would have said.
    planes: Arc<dyn PlaneSource>,
    store: Arc<S>,
    spend: Arc<dyn SpendLedger>,
    conversations: Arc<Conversations>,
    /// Every target a turn of this deployment's could actually be routed to,
    /// priced the way the router prices them.
    ///
    /// Supplied rather than quoted here, and the same list the composition
    /// root's startup cross-check is built on — see `main::reachable_candidates`
    /// and its note that a deployment attaching a fleet adds its local model to
    /// this list *at the same site it attaches the fleet*. Quoting per call was
    /// the alternative and it is worse twice over: `status` is called from a
    /// model's context, so a tool that costs a fleet round trip is a tool an
    /// agent can turn into load, and the question this list answers —
    /// [`TurnPolicy::permits`] — reads a candidate's target identity and its
    /// quality prior, neither of which a fresh quote would move.
    reachable: Vec<Candidate>,
}

impl<S: SessionStore> ControlPlaneReads<S> {
    pub fn new<P: PlaneSource>(
        planes: Arc<P>,
        store: Arc<S>,
        spend: Arc<dyn SpendLedger>,
        conversations: Arc<Conversations>,
        reachable: Vec<Candidate>,
    ) -> Self {
        Self {
            planes,
            store,
            spend,
            conversations,
            reachable,
        }
    }

    /// This principal's membership, or the refusal that says why it has none.
    ///
    /// [`MembershipError`](crate::control_config::MembershipError) is rendered
    /// as [`SurfaceError::Internal`] rather than
    /// as a tenancy answer, because both arms are facts about the *deployment's
    /// configuration* and neither is anything the calling agent did: an
    /// authenticated caller always has a membership, and one whose keys disagree
    /// is a file an operator has to go and edit. The startup cross-check is what
    /// makes the second arm unreachable in a deployment that booted.
    fn membership(&self, principal: &Principal) -> Result<crate::Admission, SurfaceError> {
        self.plane()
            .membership(principal)
            .map_err(|error| SurfaceError::Internal(error.to_string()))
    }

    /// This node's current compiled plane. See [`Self::planes`].
    fn plane(&self) -> Arc<ControlPlane> {
        self.planes.plane(self.now_ms())
    }
}

#[async_trait]
impl<S: SessionStore> ControlReads for ControlPlaneReads<S> {
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
    ) -> Result<SessionId, SurfaceError> {
        let Some(named) = conversation else {
            // The principal's most recent conversation *on this node*. A
            // principal this node has served no turn for gets the error rather
            // than somebody else's session or an empty status.
            return self
                .conversations
                .latest(principal)
                .ok_or(SurfaceError::NoSession);
        };

        // Through `qualify`, so the id an agent's `conversation` argument
        // resolves to is the id the Responses surface minted for the same cache
        // key. Two spellings of the namespace is how a namespace stops being
        // one.
        let session = self
            .conversations
            .resolve(&self.plane().qualify(principal, named));
        // Existence is the whole of the check, and it is enough because the name
        // was qualified into *this* caller's namespace first: a conversation
        // that resolves to nothing either never existed or belongs to another
        // tenant, and those two are indistinguishable from here on purpose —
        // telling them apart would make the argument an enumeration oracle, the
        // same reasoning `fetch_steer` refuses an unknown steer under.
        //
        // **On the variant and not on `Err(_)`.** That oracle argument is about
        // one question — does this session exist — and it justifies collapsing
        // only the answers to it. Every other [`StoreError`] is a fact about the
        // *store*: `RedisSessionStore::last_seq` returns
        // [`StoreError::Backend`] for a transport failure and for its
        // foreign-writer contiguity check, and rendering either as "not yours"
        // would tell an agent, in its own context, that its own conversation
        // belongs to somebody else. That is the least actionable answer
        // available — it invites a re-`init_session` or a give-up, where an
        // `Internal` invites the retry an outage actually calls for — and it is
        // wrong besides. `MemoryStore` only ever produces `SessionNotFound`,
        // which is why the catch-all read as harmless for as long as no durable
        // store was under a test.
        match self.store.last_seq(&session).await {
            Ok(_) => Ok(session),
            Err(StoreError::SessionNotFound(_)) => {
                Err(SurfaceError::ForeignConversation(named.to_string()))
            }
            Err(error) => Err(SurfaceError::Internal(error.to_string())),
        }
    }

    /// The log's end for `session`, which is what arms the surface's memo of
    /// [`session_facts`](ControlReads::session_facts).
    ///
    /// `last_seq` is monotone per session and moved by every append — exactly
    /// the cursor contract the trait states — and it is the same cheap read
    /// `resolve_session` already makes on the way to every session-scoped tool
    /// call, so the memo it enables costs nothing new. A store error becomes
    /// `Ok(None)` rather than an error: the cursor is only ever an optimization,
    /// and "answer None, project on every call" is the pre-memo behavior, never
    /// a worse one than a stale projection would be.
    async fn session_cursor(&self, session: &SessionId) -> Result<Option<u64>, SurfaceError> {
        Ok(self.store.last_seq(session).await.ok())
    }

    async fn ceiling_policy(&self, principal: &Principal) -> Result<TurnPolicy, SurfaceError> {
        Ok((*self.membership(principal)?.policy).clone())
    }

    async fn admissible_targets(
        &self,
        principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError> {
        // **Two predicates, because the engine applies two.** The policy says
        // what this key may be routed to and the credential says what it can
        // authenticate to, and a turn needs both. Asking only the first — which
        // is what this read did before M7's credential filter existed
        // downstream — makes `status` name a hosted target to a member who
        // holds no key for it, and lets `prefer`'s guard wave a narrowing onto
        // that provider through to a turn the router then withholds it from.
        // Two answers to one question, and the disagreement is invisible from
        // either side.
        let credentials = self.membership(principal)?.credentials;

        // `permits` and deliberately not `admits`: this asks what a turn of this
        // key's could *ever* be routed to, and a cadence-rationed model is one
        // it reaches on some turns. The same distinction the startup
        // cross-check draws, asked through the same predicate.
        let permitted: Vec<Candidate> = self
            .reachable
            .iter()
            .filter(|candidate| policy.permits(candidate))
            .cloned()
            .collect();

        // **A forwarding project is answered optimistically, and that is not
        // the same laxity the filter above refuses.** A member under
        // `user_only` who has attached nothing is unreachable as a fact about
        // the *file*: no request will supply the key, so naming the provider is
        // a promise every turn breaks. A pass-through project's credential is a
        // fact about one *request*, and an MCP call is not that request — the
        // configured resolution has presented nothing yet, so asking `reaches`
        // here would tell every forwarding agent it can reach nothing hosted on
        // a deployment where each of its turns can. The boot check exempts
        // forwarding for exactly this reason; see `main::unkeepable_promises`.
        // A turn that then arrives with no seat degrades to local with
        // `withheld_providers` naming the provider, which is where that answer
        // is corrected.
        let reached = match credentials.is_forwarding() {
            true => permitted,
            false => credentials.reachable(permitted).candidates,
        };
        Ok(reached
            .into_iter()
            .map(|candidate| candidate.target)
            .collect())
    }

    async fn balance(&self, principal: &Principal) -> Result<Option<Balance>, SurfaceError> {
        let Some(terms) = self.membership(principal)?.budget else {
            // No budget configured: the engine never calls the ledger for this
            // membership, so there is no position to read and none to invent.
            return Ok(None);
        };
        self.spend
            .balance(BalanceQuery {
                principal: principal.clone(),
                terms,
                now_ms: self.now_ms(),
            })
            .await
            .map(Some)
            .map_err(|error| SurfaceError::Internal(error.to_string()))
    }

    async fn session_facts(&self, session: &SessionId) -> Result<SessionFacts, SurfaceError> {
        // Lease-free, through the engine's own fold. A second projection written
        // here would be a second opinion about which steers are open, and the
        // first time it disagreed the disagreement would be invisible.
        //
        // The ledger is empty because nothing here reads it: the cache-model
        // configuration only affects `SessionState::ledger`, and this projection
        // is asked for open steers and a routing decision.
        let state = SessionState::project(self.store.as_ref(), session, CacheLedger::new(), None)
            .await
            .map_err(|error| SurfaceError::Internal(error.to_string()))?;
        Ok(SessionFacts {
            open_steers: state.open_steer_ids(),
            last_decision: state.last_decision().cloned(),
        })
    }

    fn now_ms(&self) -> u64 {
        now_ms()
    }
}

/// The path this router mounts the control surface at.
///
/// A constant because two unrelated things have to agree on it and only one of
/// them is in this file: the route below, and the `url` in the
/// `[mcp_servers.roundhouse]` stanza [`crate::codex_launch`] generates for a
/// client. A literal in each would be one edit away from a deployment whose
/// generated config points a real agent at a path this router does not serve —
/// which surfaces as a startup timeout in the agent, with nothing on our side
/// logging a miss.
pub const MCP_MOUNT_PATH: &str = "/mcp";

/// The `/mcp` route, gated by the same bearer-key resolution as the turn
/// surfaces.
///
/// Mounted with [`post_service`] rather than `route_service`, which is a
/// decision and not a detail. The SDK answers every other method `405` with an
/// `Allow: POST` of its own — see the plan's §5, where a server offering no
/// stream is permitted to — and mounting POST-only means axum refuses the same
/// methods with the same status even if the transport underneath is swapped for
/// the hand-rolled handler. The behavior a test pins is therefore a property of
/// this deployment rather than of whichever library is behind the route.
///
/// Generic over the source and stored as `Arc<dyn PlaneSource>`, rather than
/// taking the trait object directly. `Arc<ControlPlane>` and
/// `Arc<ControlDirectory>` are both accepted and both unsize at the call site;
/// a parameter typed `Arc<dyn PlaneSource>` would not accept either through the
/// `Arc::clone(&plane)` a caller naturally writes, because the clone's own
/// return type is inferred before any coercion could apply.
pub fn mcp_router<R: ControlReads, P: PlaneSource>(
    planes: Arc<P>,
    reads: Arc<R>,
    store: Arc<ControlStore>,
) -> Router {
    let planes: Arc<dyn PlaneSource> = planes;
    let surface = Arc::new(ControlPlaneSurface::new(reads, store));
    // The rebinding guard follows the mode, because what the guard is standing
    // in for differs by mode. A configured deployment is served behind whatever
    // hostname an operator gave it and refuses every request without a bearer
    // key, and a key is the one thing a rebound page cannot produce — so
    // clearing the allowlist costs nothing there. An open deployment is a
    // process on 127.0.0.1 that serves these eight tools to anyone who asks,
    // which is precisely the deployment rmcp's loopback default was written
    // for: clearing it there hands a rebound page `status`, `explain_last_route`
    // and the overlay writes against the developer's live conversation, with no
    // credential. `allowed_origins` is not an alternative — under rebinding the
    // browser believes it is same-origin.
    //
    // Resolved once here and never per request, and that is a claim rather than
    // an optimization: the mode is a property of the *deployment* — a directory
    // over a file always compiles to `Configured`, one without a file is fixed at
    // `Open`, and a fixed source cannot move at all — so no admin write and no
    // refresh can change it. A guard re-derived per request would suggest
    // otherwise.
    let hosts = match planes.plane(now_ms()).as_ref() {
        ControlPlane::Open => HostGuard::Loopback,
        ControlPlane::Configured { .. } => HostGuard::AnyHost,
    };
    Router::new()
        .route(
            MCP_MOUNT_PATH,
            post_service(roundhouse_mcp::transport::mcp_service(surface, hosts)),
        )
        .layer(axum::middleware::from_fn_with_state(planes, auth_layer))
}

/// Resolve the caller's key and put the principal where the transport reads it.
///
/// One extraction path for the whole deployment: this calls
/// [`ControlPlane::scope`] and nothing else parses a header. `KeyScope::Admin`
/// is refused as `wrong_key_kind` — an admin acts on the deployment and has no
/// membership whose routing it could narrow — and [`ControlPlane::Open`]
/// resolves every request to [`Principal::default_open`], so an unconfigured
/// deployment serves this surface exactly as it serves its turns.
async fn auth_layer(
    State(planes): State<Arc<dyn PlaneSource>>,
    mut request: Request,
    next: Next,
) -> Response {
    let principal = match planes.plane(now_ms()).scope(request.headers()) {
        Ok(KeyScope::Turn(admission)) => admission.principal,
        Ok(KeyScope::Admin) => return ApiError::from(AuthError::WrongKeyKind).into_response(),
        Err(error) => return ApiError::from(error).into_response(),
    };
    // Into the request's own extensions, which is where `rmcp` hands the
    // `http::request::Parts` to a tool call and where `RoundhouseMcp::caller`
    // looks. A tool that found none refuses rather than serving a default.
    request.extensions_mut().insert(principal);
    next.run(request).await
}

/// Every membership whose keys disagree about its entitlements, as the sentence
/// a startup refusal is built from.
///
/// Here rather than in `main` for the reason the cross-checks there are in
/// `main`: those compare two *files* and only the composition root sees both,
/// while this is a property of one control plane and of the surface that reads
/// it backwards — so it belongs beside the reader whose question it answers.
/// See [`ControlPlane::membership`].
pub fn describe_ambiguous_memberships(plane: &ControlPlane) -> Option<String> {
    let ambiguous = plane.ambiguous_memberships();
    if ambiguous.is_empty() {
        return None;
    }
    Some(format!(
        "these memberships are named by two keys with different policies or budgets, so the \
         control surface cannot say what they may do: {}. Give each key its own \
         project/user, or make the keys agree.",
        ambiguous
            .iter()
            .map(|principal| format!("`{principal}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ControlPlaneConfig;
    use crate::control_config::MembershipError;
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
        ControlPlaneReads::new(
            Arc::new(plane),
            store,
            Arc::new(MemorySpendLedger::new()),
            Arc::new(Conversations::new()),
            Vec::new(),
        )
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
        let reads = reads_over(plane, Arc::new(OutageStore));

        let error = reads
            .resolve_session(&Principal::new("acme", "ada"), Some("main"))
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
        let closed = reads_over(
            plane_with_keys(serde_json::json!([
                { "project": "acme", "user": "ada", "key_sha256": hash('a') },
            ])),
            Arc::new(MemoryStore::new()),
        );
        let refused = closed
            .resolve_session(&Principal::new("acme", "ada"), Some("main"))
            .await
            .expect_err("a session nobody created is not this caller's");
        assert!(
            matches!(refused, SurfaceError::ForeignConversation(ref named) if named == "main"),
            "{refused}"
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
}
