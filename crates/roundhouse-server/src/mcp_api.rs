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
    async fn membership(&self, principal: &Principal) -> Result<crate::Admission, SurfaceError> {
        self.plane()
            .await
            .membership(principal)
            .map_err(|error| SurfaceError::Internal(error.to_string()))
    }

    /// This node's current compiled plane. See [`Self::planes`].
    ///
    /// `async` since M16.0 (R-D1): resolving the plane may refresh it, and a
    /// refresh may be a round trip to the directory's store.
    async fn plane(&self) -> Arc<ControlPlane> {
        self.planes.plane(self.now_ms()).await
    }
}

#[async_trait]
impl<S: SessionStore> ControlReads for ControlPlaneReads<S> {
    /// **One function for both named inputs** (M12.1, R-M7). The model's
    /// `conversation` argument and the client's `_meta.threadId` are the same
    /// kind of thing — a `prompt_cache_key`-shaped name — and the only
    /// difference between them is what a failure *means*, which the shared
    /// [`resolve_session`](ControlReads::resolve_session) decides. Two copies
    /// would be two chances for one of them to skip the qualification, and a
    /// threadId resolved without it would read a bare cache key straight out
    /// of another tenant's namespace.
    ///
    /// Through `qualify`, so the id a name resolves to is the id the Responses
    /// surface minted for the same cache key. Two spellings of the namespace is
    /// how a namespace stops being one.
    async fn named_session(
        &self,
        principal: &Principal,
        named: &str,
    ) -> Result<SessionId, SurfaceError> {
        // A key nothing has bound anywhere refuses with the *same* variant an
        // unknown or another tenant's name does (M12.1 review, F9). Three
        // distinguishable answers would make the argument an enumeration
        // oracle, which the trait's own doc refuses; and the third state is
        // not "somebody else's" anyway but "never bound", which the caller
        // can do nothing with either. What it must not do is fall through to
        // generation zero: the store is shared between nodes, so that id
        // exists whenever any node minted it, and answering with it hands back
        // a log another node has already forked away from.
        //
        // A store that could not be *reached* is none of those three and is
        // rendered as an internal fault, for the reason the trait's own doc
        // gives: "not yours" about a conversation that is the caller's own is
        // both wrong and the least actionable answer available. Since M14.1
        // this is a real arm rather than a theoretical one — the generation
        // map is in the deployment's Redis.
        let Some(session) = self
            .conversations
            .resolve(&self.plane().await.qualify(principal, named))
            .await
            .map_err(|error| SurfaceError::Internal(error.to_string()))?
        else {
            return Err(SurfaceError::ForeignConversation(named.to_string()));
        };
        // Existence is the whole of the check, and it is enough because the name
        // was qualified into *this* caller's namespace first — see the trait's
        // own doc for why unknown and foreign collapse, and why every other
        // `StoreError` must not. `RedisSessionStore::last_seq` returns
        // [`StoreError::Backend`] for a transport failure and for its
        // foreign-writer contiguity check; `MemoryStore` only ever produces
        // `SessionNotFound`, which is why a catch-all read as harmless for as
        // long as no durable store was under a test.
        match self.store.last_seq(&session).await {
            Ok(_) => Ok(session),
            Err(StoreError::SessionNotFound(_)) => {
                Err(SurfaceError::ForeignConversation(named.to_string()))
            }
            Err(error) => Err(SurfaceError::Internal(error.to_string())),
        }
    }

    /// No existence check on this one, unlike [`Self::named_session`]. The
    /// binding is written at the moment the call was appended to that very log,
    /// so "the session exists" is not in question; a `last_seq` here would
    /// spend a store round trip to re-ask something *some* node observed
    /// itself.
    ///
    /// The `Result` carries only the one thing a shared table can fail with,
    /// and it is deliberately not collapsed into the `None` that means
    /// "nothing of yours" — see the trait's doc.
    async fn session_of_call(
        &self,
        principal: &Principal,
        tool_use_id: &str,
    ) -> Result<Option<SessionId>, SurfaceError> {
        self.conversations
            .session_of_call(principal, tool_use_id)
            .await
            .map_err(|error| SurfaceError::Internal(error.to_string()))
    }

    /// No existence check here either, and for [`Self::session_of_call`]'s
    /// reason: the binding is written by the ingest at the moment it decided
    /// which session that turn's history belongs to, so the session exists
    /// because the node that served that turn created it.
    async fn session_of_thread(
        &self,
        principal: &Principal,
        thread_id: &str,
    ) -> Result<Option<SessionId>, SurfaceError> {
        self.conversations
            .session_of_thread(principal, thread_id)
            .await
            .map_err(|error| SurfaceError::Internal(error.to_string()))
    }

    async fn latest_session(&self, principal: &Principal) -> Option<SessionId> {
        self.conversations.latest(principal)
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
        Ok((*self.membership(principal).await?.policy).clone())
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
        let credentials = self.membership(principal).await?.credentials;

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
        let Some(terms) = self.membership(principal).await?.budget else {
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
        // here would be a second opinion about what roundhouse last said to this
        // conversation, and the first time it disagreed the disagreement would
        // be invisible.
        //
        // **Since M10.0 this is the only way to answer `fetch_steer` at all.**
        // The guidance used to be a node-local deposit keyed by the synthetic
        // call's id, so serving it needed no log read and lost it on restart.
        // The correction is a conversation item now and the fold is what says
        // which item it is — which is why the tool became a read of the session
        // rather than of a store.
        //
        // The ledger is empty because nothing here reads it: the cache-model
        // configuration only affects `SessionState::ledger`, and this projection
        // is asked for the last guidance and the last routing decision.
        let state = SessionState::project(self.store.as_ref(), session, CacheLedger::new(), None)
            .await
            .map_err(|error| SurfaceError::Internal(error.to_string()))?;
        Ok(SessionFacts {
            latest_guidance: state.last_guidance().map(str::to_string),
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
pub async fn mcp_router<R: ControlReads, P: PlaneSource>(
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
    let hosts = match planes.plane(now_ms()).await.as_ref() {
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
    let principal = match planes.plane(now_ms()).await.scope(request.headers()) {
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
mod tests;
