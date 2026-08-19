// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The tool contract: one trait, one request type and one response type per
//! tool.
//!
//! Everything a client can ask this deployment is a method here, and everything
//! it can be told back is a `#[derive(Serialize)]` struct here. The transport
//! ([`crate::transport`]) contributes nothing to the vocabulary — which is the
//! point, because it means the whole surface is testable by calling functions.
//!
//! # One text block, no `structuredContent`
//!
//! [`ToolOutcome`] can hold exactly one text block, and that is a type-level
//! statement of a wire decision rather than a convenience. MCP lets a tool
//! answer with both an unstructured `content` array and a structured
//! `structuredContent` object; a client that renders one and a server that
//! means the other disagree silently. Worse for us specifically: the tool
//! output travels back into a session as a conversation item, our canonicalizer
//! round-trips a tool result through its `Value::String` branch, and a
//! structured object would take a different path through it — so the bytes the
//! client resends next turn would not be the bytes we emitted, and the prefix
//! would fork. One text block is the shape that survives the round trip.
//!
//! The text is JSON, pretty-printed with the field order of the response
//! struct. JSON *inside* the text block rather than beside it: an agent parses
//! it exactly as reliably, and it never leaves the branch above.
//!
//! # Errors are outcomes, not transport failures
//!
//! A refused tool call is a [`ToolOutcome`] with `is_error` set, not a JSON-RPC
//! error — that is what the MCP specification asks for, and it is also what
//! keeps a refused `report_outcome` from looking like a broken connection to a
//! client that is mid-turn. [`SurfaceError`] renders into one, in one place.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use roundhouse_core::control::Principal;

use crate::overlay::{OverlayScope, PreferMode};

/// Everything an MCP client can ask of a roundhouse deployment.
///
/// `principal` is resolved by the transport from the same `Authorization:
/// Bearer rh_turn_…` header the turn surfaces use, so a tool can never be
/// called without one and never has to ask whose deployment it is looking at.
#[async_trait]
pub trait ControlSurface: Send + Sync + 'static {
    /// What this key may be routed to right now, and what is left to spend.
    async fn status(
        &self,
        principal: &Principal,
        request: StatusRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// Mint an id that identifies this MCP connection's conversation.
    ///
    /// See [`InitSessionResponse`] for why the answer is a string the client is
    /// asked to keep rather than a header.
    async fn init_session(
        &self,
        principal: &Principal,
        request: InitSessionRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// Record what the agent is trying to do. Changes no routing.
    async fn declare_intent(
        &self,
        principal: &Principal,
        request: DeclareIntentRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// Ask for local, frontier, or neither, for a while.
    async fn prefer(
        &self,
        principal: &Principal,
        request: PreferRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// Raise the quality floor this session's turns are routed under.
    async fn set_quality_floor(
        &self,
        principal: &Principal,
        request: SetQualityFloorRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// Read the corrective payload a synthetic tool call named.
    async fn fetch_steer(
        &self,
        principal: &Principal,
        request: FetchSteerRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// Say what happened to a steer. Advisory; never blocks anything.
    async fn report_outcome(
        &self,
        principal: &Principal,
        request: ReportOutcomeRequest,
    ) -> Result<ToolOutcome, SurfaceError>;

    /// The last routing decision for this conversation, agent-readable.
    async fn explain_last_route(
        &self,
        principal: &Principal,
        request: ExplainLastRouteRequest,
    ) -> Result<ToolOutcome, SurfaceError>;
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Which conversation a session-scoped tool concerns.
///
/// The client's own `prompt_cache_key`, resolved through the same
/// `bound_session` namespacing the Responses surface uses, so the two surfaces
/// agree by construction. Omitted, it means the principal's most recent
/// session — which is the right default for the overwhelmingly common case of
/// one agent, one conversation, and is wrong loudly rather than quietly for the
/// rest: a principal with no session at all is an error naming that fact, not
/// an empty status.
///
/// A type alias rather than a shared struct because every request already needs
/// its own serde shape for the tool schema, and a `#[serde(flatten)]` common
/// half is exactly the thing that makes a hand-written JSON Schema stop
/// matching the type it describes.
pub type Conversation = Option<String>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    #[serde(default)]
    pub conversation: Conversation,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitSessionRequest {
    #[serde(default)]
    pub conversation: Conversation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclareIntentRequest {
    pub goal: String,
    #[serde(default)]
    pub plan_steps: Vec<String>,
    pub done_when: String,
    #[serde(default)]
    pub conversation: Conversation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferRequest {
    pub mode: PreferMode,
    pub scope: OverlayScope,
    /// How many turns the preference lasts, for a session-scoped ask.
    ///
    /// Absent under [`OverlayScope::Session`] means "until it is replaced".
    /// Under [`OverlayScope::Turn`] the only value that is not a contradiction
    /// is `1`, and anything else is refused rather than silently ignored — a
    /// dropped field is how an agent comes to believe a preference it does not
    /// have.
    #[serde(default)]
    pub turns: Option<u32>,
    /// Why. Required, and stored: an unexplained routing change is unauditable.
    pub reason: String,
    #[serde(default)]
    pub conversation: Conversation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetQualityFloorRequest {
    /// The lowest `quality_prior` this session's turns may be routed to, on the
    /// same `0.0..=1.0` scale the catalog states priors on.
    pub floor: f64,
    pub turns: u32,
    pub reason: String,
    #[serde(default)]
    pub conversation: Conversation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchSteerRequest {
    pub steer_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerOutcome {
    Applied,
    Rejected,
    NotApplicable,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportOutcomeRequest {
    pub steer_id: String,
    pub outcome: SteerOutcome,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplainLastRouteRequest {
    #[serde(default)]
    pub conversation: Conversation,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// What `status` says.
///
/// **Names, never prices.** An agent that can see what a model costs can argue
/// about what it costs, and the argument is with a component that has no way to
/// check whether the agent is quoting its own context back at it. The budget
/// figures below are the deliberate exception and they are per-*membership*,
/// not per-model: knowing three dollars are left tells an agent to wrap up,
/// while knowing which model is dearest tells it what to lobby for.
#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub conversation: String,
    /// Fingerprint of the policy the next turn will be routed under, overlay
    /// included. The same string the next `DecisionRecord` will carry, which is
    /// what makes an overlay's effect checkable from the audit trail rather
    /// than only from this tool.
    pub policy_digest: String,
    /// Every target the effective policy admits, by
    /// [`policy_identity`](roundhouse_core::routing::Target::policy_identity).
    pub admissible_targets: Vec<String>,
    /// `None` on a deployment that meters nothing — see
    /// [`ControlReads::balance`](crate::reads::ControlReads::balance). Absent
    /// rather than zeroed: an agent reading a budget field wants to know
    /// whether to wrap up, and the honest answer where no ceiling exists is
    /// that the question does not apply.
    pub budget: Option<BudgetView>,
    /// Steers this deployment has emitted that no turn has answered yet.
    pub open_steers: Vec<String>,
    /// The agent's own standing narrowing, if it has one.
    pub overlay: Option<OverlayView>,
}

/// Budget remaining, stamped with the basis it was read on.
///
/// `basis` is `"committed"` and, until M6's validate loop produces measured
/// usage of its own, only ever `"committed"` — the number is
/// [`SpendLedger::balance`](roundhouse_core::control::SpendLedger::balance)'s
/// answer, which is settled spend plus live holds. A `"measured"` basis exists
/// in the design and has no producer yet, so it has no field here either: a
/// field with no producer is a lie the first reader believes.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetView {
    pub basis: &'static str,
    pub project_remaining_usd: f64,
    /// `None` for a pooled membership — there is no second ceiling, which is
    /// not the same as a ceiling of zero.
    pub member_remaining_usd: Option<f64>,
    /// `unconstrained`, `warned`, or `exhausted`.
    ///
    /// Spelled here rather than serialized from
    /// [`LedgerState`](roundhouse_core::control::LedgerState) because the tool
    /// vocabulary is this crate's contract with an agent, and a ledger enum
    /// gaining a variant should be a compile error here rather than a new word
    /// appearing in an agent's context unannounced.
    pub state: &'static str,
}

/// The standing overlay, as `status` and the two overlay writers render it.
#[derive(Debug, Clone, Serialize)]
pub struct OverlayView {
    pub mode: Option<PreferMode>,
    pub mode_reason: Option<String>,
    pub mode_turns_remaining: Option<u32>,
    pub quality_floor: Option<f64>,
    pub floor_reason: Option<String>,
    pub floor_turns_remaining: Option<u32>,
}

/// What `prefer` and `set_quality_floor` say.
#[derive(Debug, Clone, Serialize)]
pub struct OverlayResponse {
    pub conversation: String,
    /// Whether the ask was honored in full.
    ///
    /// `true` means the deployment's ceiling gave you less than you asked for —
    /// either because the ask would have *widened* the admissible set, which a
    /// narrowing overlay cannot do, or because it would have emptied it. Never
    /// an error: an agent that gets an error for asking has to guess, and an
    /// agent that guesses asks again.
    pub narrowed: bool,
    /// Present when `narrowed`: what the ceiling did instead, in a sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrowed_because: Option<&'static str>,
    /// The policy the *next* turn will be routed under, after this overlay.
    pub policy_digest: String,
    pub admissible_targets: Vec<String>,
    pub overlay: Option<OverlayView>,
}

/// What `init_session` says.
///
/// # Why an id in the output text
///
/// An MCP connection cannot carry the client's conversation id: Codex sources
/// `[mcp_servers.*]` headers from static config and environment variables, so
/// there is no header we could ask it to set per conversation. The correlation
/// therefore runs the other way. The minted id goes out in this tool's *output*,
/// the client appends that output to its conversation as an ordinary item, and
/// the next turn's resent history carries it into the session log — where
/// [`binding_in_items`](crate::store::binding_in_items) finds it. The session
/// whose log holds the id is the session that made the call, provable from the
/// log alone.
///
/// [`Self::note`] is what makes that work: the client must be told, in the
/// output it is about to append, that the id identifies this session. Without
/// it a summarizing client drops the id as noise and the join never happens.
#[derive(Debug, Clone, Serialize)]
pub struct InitSessionResponse {
    pub session_binding_id: String,
    pub conversation: String,
    pub note: &'static str,
}

/// What `declare_intent` says: the stored record, echoed.
#[derive(Debug, Clone, Serialize)]
pub struct IntentResponse {
    pub conversation: String,
    pub goal: String,
    pub plan_steps: Vec<String>,
    pub done_when: String,
    /// Deliberately flat: an intent changes no routing, and saying otherwise
    /// here would be the one place in the surface that lies about what a tool
    /// did.
    pub routing_effect: &'static str,
}

/// What `fetch_steer` says.
///
/// Every field is read from a record committed when the steer was emitted, so
/// two calls produce identical bytes and neither does any work a provider could
/// bill for. That is not an optimization: a handler that ran the judge on
/// invocation would let a model — or a prompt injection reading this very
/// description — drain the validate budget by calling the tool in a loop.
#[derive(Debug, Clone, Serialize)]
pub struct SteerResponse {
    pub steer_id: String,
    pub guidance: String,
    pub emitted_at_ms: u64,
}

/// What `report_outcome` says.
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeResponse {
    pub steer_id: String,
    pub outcome: SteerOutcome,
    pub recorded: bool,
}

/// What `explain_last_route` says.
///
/// The audit trail as a tool, minus the money. `considered` carries names
/// because the counterfactual is the useful half — "it could have gone
/// somewhere else and did not" — and carries no prices for the reason
/// [`StatusResponse`] states.
#[derive(Debug, Clone, Serialize)]
pub struct RouteExplanation {
    pub conversation: String,
    pub chosen: String,
    pub rationale: String,
    /// The name of the deployment's routing policy — how the choice was made,
    /// as distinct from what it was allowed to choose from.
    pub routing_policy: String,
    pub budget_state: roundhouse_core::control::BudgetState,
    pub turn_policy_digest: String,
    pub considered: Vec<String>,
}

// ---------------------------------------------------------------------------
// Outcomes and errors
// ---------------------------------------------------------------------------

/// One tool result: exactly one text block, and whether it is an error.
///
/// The invariant is the type. There is no constructor that takes a content
/// array and none that takes a structured object, so "a tool answered with two
/// blocks" is not a state this crate can reach — see the module docs for why
/// that matters to the conversation prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    text: String,
    is_error: bool,
}

impl ToolOutcome {
    /// Render a response type into the one text block.
    ///
    /// Pretty-printed rather than compact because the text lands in a model's
    /// context, where the newlines cost a handful of tokens and buy a shape the
    /// model reads without a parser. Deterministic either way: field order is
    /// struct order, and no response type here contains a map.
    pub fn ok<T: Serialize>(value: &T) -> Result<Self, SurfaceError> {
        Ok(Self {
            text: serde_json::to_string_pretty(value)
                .map_err(|error| SurfaceError::Internal(error.to_string()))?,
            is_error: false,
        })
    }

    /// Render a refusal into the one text block.
    pub fn refused(error: &SurfaceError) -> Self {
        Self {
            text: error.to_string(),
            is_error: true,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// The MCP `CallToolResult` body, as the transport puts it on the wire.
    ///
    /// Built here rather than in [`crate::transport`] so the shape is pinnable
    /// without a socket, and so that swapping the transport cannot quietly
    /// change what a client receives.
    pub fn to_call_tool_json(&self) -> serde_json::Value {
        serde_json::json!({
            "content": [{ "type": "text", "text": self.text }],
            "isError": self.is_error,
        })
    }
}

/// Why a tool call was refused.
///
/// # The two that read alike on purpose
///
/// An unknown `steer_id` and another principal's `steer_id` produce the *same*
/// variant with the same rendering. Telling them apart would turn the tool into
/// an oracle: a caller could enumerate ids and learn which ones exist in some
/// other tenant's session, which is a slow leak of exactly the fact tenancy
/// exists to hide. So `fetch_steer` resolves the id, compares principals, and —
/// when either check fails — says only that this caller has no such steer.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("no tool named `{0}` is served here")]
    UnknownTool(String),
    #[error("`{tool}` arguments are not valid: {detail}")]
    BadArguments { tool: &'static str, detail: String },
    /// A field that is present but empty, or present and self-contradictory.
    ///
    /// Named separately from [`Self::BadArguments`] because serde's message
    /// covers *absent* and *ill-typed*, and cannot cover "you sent an empty
    /// reason" — which is the failure an agent actually makes when a tool
    /// requires a justification it does not have.
    #[error("`{field}` is required and must {requirement}")]
    InvalidField {
        field: &'static str,
        requirement: &'static str,
    },
    #[error("no steer `{steer_id}` belongs to this key")]
    UnknownSteer { steer_id: String },
    #[error("this key has no conversation yet; start a turn before asking about one")]
    NoSession,
    #[error("conversation `{0}` does not belong to this key")]
    ForeignConversation(String),
    #[error("conversation `{0}` has not been routed yet")]
    NotRoutedYet(String),
    #[error("the control plane could not answer: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for SurfaceError {
    fn from(error: anyhow::Error) -> Self {
        SurfaceError::Internal(error.to_string())
    }
}
