// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The turn engine: the composition root where state, context, routing, and
//! execution meet.
//!
//! One turn is: claim the session, replay it into a context assembler, admit
//! the turn, price every target, reserve what may be spent, choose, record the
//! choice, execute, and settle — the log first, then the money. Each of those
//! steps lives in a lower layer; this module only sequences them.
//!
//! The sequencing itself carries three guarantees worth stating. The routing
//! decision is written to the log *before* execution starts, so the audit trail
//! records what was chosen even when execution then fails. The spend ledger is
//! written *after* the terminal event, so what a project is charged is priced
//! from what the log actually holds rather than from what a turn intended. And
//! a booked local reservation is always settled, including on the error path,
//! because a leaked reservation silently inflates the router's view of a worker
//! forever.
//!
//! [`http`] puts a transport in front of that sequence without joining it: it
//! streams turns by tailing the same log the engine writes to, so the wire
//! protocol has no state of its own to keep in agreement.
//!
//! [`responses_api`] is a second transport over that same log, speaking the
//! OpenAI Responses API so existing agents can drive Roundhouse unmodified. It
//! adds no state either: a client's resent conversation is checked against the
//! log as a prefix rather than remembered alongside it.
//!
//! [`messages_api`] is the third framing of that log, speaking the Anthropic
//! Messages API so Claude Code drives Roundhouse unmodified. It is a sibling of
//! [`responses_api`] rather than a layer on it — same prefix admission, same
//! follower shape, a different vocabulary in each direction — and it carries
//! the strictness that client's parser demands: an SSE frame without an
//! `event:` line is dropped in silence, and a dropped stream costs a second
//! full-price non-streaming turn.
//!
//! [`mcp_api`] is the fourth, and the only one an *agent* rather than a client
//! drives: it mounts the control tools in [`roundhouse_mcp`] behind the same key
//! resolution as the rest, so a model can read what it is being routed to and
//! ask to be routed to less. It adds no state of its own either — its writes go
//! to a node-local control store the engine reads at the start of every turn.
//!
//! [`admin_api`] is the fifth, and the only one that *writes*: it is the
//! surface an operator creates projects, members and keys through, and the
//! reason the other four hold a [`ControlDirectory`] rather than a compiled
//! [`ControlPlane`] — a key revoked there has to stop working on all of them,
//! which it cannot do if each captured its own plane at mount time.
//!
//! [`relay_api`] is the sixth, and the only one that speaks somebody else's
//! vocabulary: three reads that project the same log into NeMo Relay's published
//! formats — ATOF, ATIF and `LlmOptimizationSummary` — so a deployment
//! interoperating with Relay's ecosystem is not instrumented twice. It holds no
//! state and never touches the engine; see [`roundhouse_relay`].
//!
//! [`metrics_api`] reports on all of it. Token counts, dollars, and the savings
//! figure are folded out of the same log as everything else — see
//! [`roundhouse_core::metrics`] — so the dashboard cannot disagree with the
//! audit trail it summarizes.
//!
//! [`codex_launch`] faces the other way from all six: it *writes* the config an
//! unmodified Codex reads in order to point at this deployment. It is here
//! because the stanza needs the bound address, the turn-key header name and the
//! MCP mount path at once, and this crate is the only one that knows all three.
//!
//! [`claude_launch`] is its sibling for the other client, and the asymmetry
//! between them is the client's rather than ours: Claude Code's whole redirect
//! surface is environment, so that module writes no file at all and its output
//! is a map a launcher hands the child process.

pub mod admin_api;
pub mod catalog_config;
pub mod claude_launch;
pub mod codex_launch;
pub mod control_config;
pub mod conversations;
pub mod dialect;
pub mod engine;
pub mod http;
pub mod judge;
pub mod mcp_api;
pub mod messages_api;
pub mod metrics_api;
pub mod prefix_admission;
pub mod relay_api;
pub mod relay_handoff;
pub mod responses_api;
pub mod shared_backend;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod tokenizer;

pub use admin_api::admin_router;
pub use catalog_config::{CatalogConfig, CatalogError};
pub use claude_launch::{ClaudeAuthKind, ClaudeEnv, ClaudeLaunch, ClaudeLaunchError};
pub use codex_launch::{CodexAuthKind, CodexLaunch};
pub use control_config::{
    Admission, AuthError, ControlDirectory, ControlPlane, ControlPlaneConfig, ControlPlaneError,
    CrossChecks, DirectoryError, DirectoryMutation, DirectoryStore, DirectoryView, KeyScope,
    MembershipError, MemoryDirectoryStore, PlaneSource, ValidateConfig, has_valid_key_shape,
};
pub use conversations::Conversations;
pub use dialect::{ClientDialect, DEFAULT_MCP_NAMESPACE};
pub use engine::{
    EchoLocalExecutor, Engine, EngineConfig, EngineError, LocalExecution, LocalExecutor, TurnInput,
    TurnResult,
};
pub use http::router;
pub use judge::{FleetJudge, JudgeConfig};
pub use mcp_api::{ControlPlaneReads, describe_ambiguous_memberships, mcp_router};
pub use messages_api::messages_router;
pub use metrics_api::metrics_router;
pub use relay_api::relay_router;
pub use relay_handoff::{RelayAgent, RelayHandoff, RelayHandoffError, UpstreamReAimed};
pub use responses_api::{API_PREFIX, responses_router};
pub use shared_backend::{
    Backends, REDIS_NAMESPACE_VAR, REDIS_VAR, SharedBackend, resolve_namespace, shared_backend,
};
pub use tokenizer::HfTokenizer;
