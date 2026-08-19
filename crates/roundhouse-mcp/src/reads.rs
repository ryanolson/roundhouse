// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Everything the surface reads about a running deployment.
//!
//! One trait, implemented by `roundhouse-server` — which is where a control
//! plane, a catalog, a spend ledger and a session store are all visible at
//! once, and where this crate must never depend. Every method is a read; there
//! is no write here, because the only writes the surface performs go to
//! [`ControlStore`](crate::store::ControlStore).
//!
//! # Why the admissibility question lives behind the seam
//!
//! [`Self::admissible_targets`] takes a [`TurnPolicy`] and answers with targets
//! rather than handing this crate a catalog to filter itself. The alternative —
//! a `catalog()` read plus a local filter — needs a
//! [`Candidate`](roundhouse_core::routing::Candidate) to call
//! [`TurnPolicy::permits`], and the only Candidate available here would be one
//! fabricated with zero prices and a made-up prefill estimate. That is a second
//! opinion about admissibility built out of numbers nobody measured, and the
//! first time it disagreed with the router the disagreement would show up as a
//! `status` that promised a target the next turn refused.
//!
//! `admits_when_spent` in core sets the precedent and states the rule: when a
//! question cannot be asked honestly from outside, the question moves to where
//! it can be.

use async_trait::async_trait;

use roundhouse_core::control::{Balance, Principal, TurnPolicy};
use roundhouse_core::ids::SessionId;
use roundhouse_core::routing::{DecisionRecord, Target};

use crate::surface::SurfaceError;

/// What a session's log says, projected.
///
/// Both fields are folds of committed events, which is what keeps the tools
/// that render them pure reads: nothing here is derived from a table this crate
/// maintains beside the log.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    /// `call_id`s of steers this deployment emitted that no turn has answered.
    pub open_steers: Vec<String>,
    /// The most recent routing decision, or `None` for a session whose first
    /// turn has not been routed yet.
    pub last_decision: Option<DecisionRecord>,
}

#[async_trait]
pub trait ControlReads: Send + Sync + 'static {
    /// Which conversation this call concerns.
    ///
    /// `conversation` is the client's own `prompt_cache_key`, resolved through
    /// the same namespacing the Responses surface uses. Omitted, the
    /// principal's most recent session. Two failures are distinct and both are
    /// errors rather than defaults: a principal with no session at all is
    /// [`SurfaceError::NoSession`], and a named conversation outside the
    /// caller's namespace is [`SurfaceError::ForeignConversation`] — never a
    /// silent fall back to the caller's own most recent one, which would let a
    /// probe for someone else's key read as an ordinary answer.
    async fn resolve_session(
        &self,
        principal: &Principal,
        conversation: Option<&str>,
    ) -> Result<SessionId, SurfaceError>;

    /// The policy this principal's turns are admitted under, before any
    /// overlay: project profile composed with the membership's overrides.
    async fn ceiling_policy(&self, principal: &Principal) -> Result<TurnPolicy, SurfaceError>;

    /// Every target in the deployment's catalog that `policy` admits.
    ///
    /// A catalog read and a policy filter, not a quote: this must never reach
    /// the fleet, because `status` is called from a model's context and a tool
    /// that costs a round trip per call is a tool an agent can turn into load.
    async fn admissible_targets(
        &self,
        principal: &Principal,
        policy: &TurnPolicy,
    ) -> Result<Vec<Target>, SurfaceError>;

    /// This membership's position against its project and member ceilings.
    ///
    /// `None` when the membership has no budget configured at all — an open
    /// deployment, or a project whose file names no `"budget"`. That is a
    /// distinct answer from a balance of zero and it has to stay distinct here,
    /// because the engine treats it as one: an admission with no budget never
    /// calls the ledger, so there is no position to report. Rendering it as
    /// "0.00 remaining" would tell an agent to wrap up on a deployment that
    /// meters nothing, and rendering it as an enormous number would put a
    /// figure nobody wrote in a model's context.
    async fn balance(&self, principal: &Principal) -> Result<Option<Balance>, SurfaceError>;

    /// What `session`'s log projects to.
    ///
    /// The expensive read on this trait: on a real deployment it is a replay of
    /// the whole log. [`Self::session_cursor`] is what lets the surface skip it.
    async fn session_facts(&self, session: &SessionId) -> Result<SessionFacts, SurfaceError>;

    /// Where `session`'s log ends, if that can be answered without replaying it.
    ///
    /// A projection of a log that has not advanced cannot have changed, so this
    /// is the whole freshness check behind the surface's memo of
    /// [`Self::session_facts`] — see `ControlPlaneSurface::session_facts` for
    /// why a memo is needed at all (`status` and `explain_last_route` are
    /// called from a model's context, and a model can call a tool in a loop).
    ///
    /// The default answers `None`, meaning "not cheaply" — and a `None` makes
    /// the surface project on every call, which is exactly what every caller
    /// paid before the memo existed. It is a default rather than a required
    /// method because a cheap cursor is a property of the *store* behind an
    /// implementation and not of the seam: an implementation over a durable
    /// session store has one, and a test double may not care to have one.
    ///
    /// `roundhouse-server`'s `ControlPlaneReads` overrides this over the
    /// `SessionStore::last_seq` that `resolve_session` already calls on every
    /// session-scoped tool call, so on the shipped deployment a repeat `status`
    /// or `explain_last_route` between turns is one cursor read rather than a
    /// full log replay. The default here stays `None` for implementations —
    /// test doubles among them — whose store cannot answer the cursor cheaply.
    ///
    /// A cursor must be *monotone per session and moved by every append*. A
    /// value that repeats across two different log states would serve a stale
    /// projection, which is a worse failure than the cost this exists to avoid:
    /// answer `None` rather than something approximate.
    async fn session_cursor(&self, session: &SessionId) -> Result<Option<u64>, SurfaceError> {
        let _ = session;
        Ok(None)
    }

    /// Wall-clock milliseconds, as the deployment tells the time.
    ///
    /// Here rather than behind a third seam of its own. A clock is a fact about
    /// the deployment exactly as a catalog and a ledger are, the two records
    /// this crate stamps with it — a declared intent and a minted binding — are
    /// only ever compared against timestamps the same deployment wrote, and a
    /// separate `Clock` trait would be a second thing every implementor and
    /// every test double has to supply for one integer.
    fn now_ms(&self) -> u64;
}
