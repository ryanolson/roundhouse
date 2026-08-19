// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The roundhouse control surface, as an MCP server.
//!
//! An agent talking to roundhouse over the Responses API can see what it is
//! being routed to only by inference. This crate gives it eight tools that say
//! so directly — and, for two of them, let it ask for *less*.
//!
//! # Three properties hold the whole design up
//!
//! **No tool appends to a session log.** An MCP request arrives on its own HTTP
//! request, and a session log has exactly one writer at a time — the turn gate
//! within a process, the store's lease across them. A second writer would
//! contend with both. So every tool here is either a pure read of committed
//! state or a write to the node-local [`ControlStore`], and steer fulfilment is
//! a *projection* of the ordinary write path rather than something this crate
//! performs. That is what lets a handler stay a pure reader of a stateful loop.
//!
//! **Overlays narrow and never widen.** [`prefer`](ControlSurface::prefer) and
//! [`set_quality_floor`](ControlSurface::set_quality_floor) are exposed to a
//! model, and a model reading its own context is one prompt injection away from
//! being someone else's. They are safe to expose because
//! [`TurnPolicy::narrow`](roundhouse_core::control::TurnPolicy::narrow) is
//! total and can only shrink the admissible set: an overlay that asks for more
//! than the deployment's ceiling allows is *clamped and reported*, never
//! honored and never refused. See [`overlay`] for the second half of that rule
//! — the one the narrow machinery cannot state, which is that an overlay
//! leaving nothing admissible is also an over-ask.
//!
//! **The transport is one file.** The tool semantics live in
//! [`ControlSurface`], a trait over plain serde request/response types, and are
//! tested against it with no socket in sight. [`transport`] binds that trait to
//! the official `rmcp` SDK and holds every line of code that knows what
//! JSON-RPC is. Swapping it for a hand-rolled handler moves no test.
//!
//! # Dependency direction
//!
//! `roundhouse-mcp` depends on `roundhouse-core` and on nothing else of ours.
//! It must never depend on `roundhouse-server`, which is the crate that
//! *supplies* the two seams below and mounts the router. Everything the surface
//! needs to read about a deployment arrives through [`ControlReads`]; everything
//! it writes goes to [`ControlStore`].
//!
//! # A documented assumption, pending M9
//!
//! Nothing in this crate has been exercised against a real `codex` binary. Two
//! facts are taken from the pinned Codex source and are load-bearing for the
//! surface to be reachable at all:
//!
//! - a `[mcp_servers.*]` entry with `url` speaks streamable HTTP and sends its
//!   bearer from `bearer_token_env_var` (`codex-rs/mcp-client/src/router.rs:164`
//!   selects the transport by config shape);
//! - a tool named in the client's registry is dispatched by the client and its
//!   output appended to the conversation as an ordinary item
//!   (`codex-rs/core/src/mcp/registry.rs:440-444`).
//!
//! The first is what makes the endpoint speak to Codex at all; the second is
//! what will make [`init_session`](ControlSurface::init_session)'s correlation
//! trick work, since the minted id reaches a session log only by riding the
//! client's own resent history. M9's real-binary end-to-end is the test that
//! closes both; until it is green this block is the honest statement of what we
//! have not proven.
//!
//! Note the tense. M5 ships the *write* half of that trick — an id minted,
//! recorded and returned in a form a client keeps — and nothing in the
//! deployment resolves a session from a binding yet. The read side is M7's, per
//! the plan's §3, and both agent-facing sentences about it
//! ([`tools::descriptors`] and [`surface::InitSessionResponse::note`]) are
//! written to say what is recorded rather than what is correlated.

pub mod overlay;
pub mod reads;
pub mod store;
pub mod surface;
pub mod tools;
pub mod transport;

mod plane;

pub use overlay::{ModeNarrowing, OverlayScope, PreferMode, SessionOverlay, TimedOverlay};
pub use plane::ControlPlaneSurface;
pub use reads::{ControlReads, SessionFacts};
pub use store::{
    BindingId, ControlStore, IntentRecord, SessionBinding, SteerRecord, binding_ids_in_items,
    binding_in_items,
};
pub use surface::{ControlSurface, SurfaceError, ToolOutcome};
pub use tools::{TOOL_NAMES, ToolCall, ToolDescriptor, descriptors, dispatch};
