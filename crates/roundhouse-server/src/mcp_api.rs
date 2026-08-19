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
use roundhouse_core::store::SessionStore;
use roundhouse_mcp::reads::{ControlReads, SessionFacts};
use roundhouse_mcp::surface::SurfaceError;
use roundhouse_mcp::{ControlPlaneSurface, ControlStore};

use crate::control_config::{AuthError, ControlPlane, KeyScope};
use crate::conversations::Conversations;
use crate::http::ApiError;

/// Everything the control surface reads a running deployment through.
///
/// Assembled at the composition root from the same values the turn surfaces
/// were built with — one control plane, one store, one ledger, one conversation
/// table — because a second copy of any of them would be a control surface that
/// reports on a deployment adjacent to the one serving turns.
pub struct ControlPlaneReads<S: SessionStore> {
    plane: Arc<ControlPlane>,
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
    pub fn new(
        plane: Arc<ControlPlane>,
        store: Arc<S>,
        spend: Arc<dyn SpendLedger>,
        conversations: Arc<Conversations>,
        reachable: Vec<Candidate>,
    ) -> Self {
        Self {
            plane,
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
        self.plane
            .membership(principal)
            .map_err(|error| SurfaceError::Internal(error.to_string()))
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
            .resolve(&self.plane.qualify(principal, named));
        // Existence is the whole of the check, and it is enough because the name
        // was qualified into *this* caller's namespace first: a conversation
        // that resolves to nothing either never existed or belongs to another
        // tenant, and those two are indistinguishable from here on purpose —
        // telling them apart would make the argument an enumeration oracle, the
        // same reasoning `fetch_steer` refuses an unknown steer under.
        match self.store.last_seq(&session).await {
            Ok(_) => Ok(session),
            Err(_) => Err(SurfaceError::ForeignConversation(named.to_string())),
        }
    }

    async fn ceiling_policy(&self, principal: &Principal) -> Result<TurnPolicy, SurfaceError> {
        Ok((*self.membership(principal)?.policy).clone())
    }

    async fn admissible_targets(
        &self,
        _principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError> {
        // `permits` and deliberately not `admits`: this asks what a turn of this
        // key's could *ever* be routed to, and a cadence-rationed model is one
        // it reaches on some turns. The same distinction the startup
        // cross-check draws, asked through the same predicate.
        Ok(self
            .reachable
            .iter()
            .filter(|candidate| policy.permits(candidate))
            .map(|candidate| candidate.target.clone())
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
pub fn mcp_router<R: ControlReads>(
    plane: Arc<ControlPlane>,
    reads: Arc<R>,
    store: Arc<ControlStore>,
) -> Router {
    let surface = Arc::new(ControlPlaneSurface::new(reads, store));
    Router::new()
        .route(
            "/mcp",
            post_service(roundhouse_mcp::transport::mcp_service(surface)),
        )
        .layer(axum::middleware::from_fn_with_state(plane, auth_layer))
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
    State(plane): State<Arc<ControlPlane>>,
    mut request: Request,
    next: Next,
) -> Response {
    let principal = match plane.scope(request.headers()) {
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

    /// A one-project plane whose `keys` array is whatever the caller writes.
    fn plane_with_keys(keys: serde_json::Value) -> ControlPlane {
        let json = serde_json::json!({
            "projects": [{ "id": "acme", "policy": { "min_quality": 0.1 } }],
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
