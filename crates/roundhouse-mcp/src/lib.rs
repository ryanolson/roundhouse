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
//! # Verified against a real binary
//!
//! Two facts about the client hold this surface up: that a `[mcp_servers.*]`
//! entry with a `url` speaks streamable HTTP and sends its bearer from
//! `bearer_token_env_var`, and that a tool the client resolves is dispatched
//! and its output appended to the conversation as an ordinary item. Both were
//! read out of Codex's source until M9. Both are now observed against the
//! binary an operator actually runs — `codex-cli 0.146.0`, tree `e363b08` —
//! by `crates/roundhouse-server/tests/codex_e2e.rs`, which drives the real
//! process against a real roundhouse over a real socket.
//!
//! - **The endpoint is reachable, keyed, and speaks our protocol.**
//!   `McpServerTransportConfig::StreamableHttp { url, bearer_token_env_var, … }`
//!   (`config/src/mcp_types.rs:449-463` @ `e363b08`) is selected by config
//!   shape, and the token is read from the environment rather than from the
//!   file. Proved by `a_real_codex_binary_completes_the_mcp_handshake_against_our_server`:
//!   codex's `initialize` and `tools/list` arrive at our mount carrying the
//!   minted turn key as `Authorization: Bearer …`, negotiate protocol version
//!   `2025-06-18` — which is exactly what [`transport`] declares — and
//!   `fetch_steer` is in the tool list that comes back. That test settles
//!   something no source reading could: rmcp 3.1.3 serving an rmcp 1.8.0
//!   client, a pairing nothing had ever exercised.
//!
//! - **A dispatched tool's output rides back into the conversation.** Codex
//!   builds the namespace it dispatches on as `mcp__{server table key}`
//!   (`codex-mcp/src/tools.rs:22,228-234`) and resolves a call on the exact
//!   `(namespace, name)` pair (`core/src/tools/handlers/mcp.rs:29-66,121`).
//!   Proved by `a_real_codex_binary_executes_our_synthetic_tool_call_and_returns_its_output`
//!   and `a_real_codex_binary_resends_the_call_and_output_and_the_session_does_not_fork`:
//!   the client dispatched a synthetic `fetch_steer` it had never been told
//!   about, appended the result, and resent the call with its `arguments`
//!   byte-identical and its `function_call_output` immediately after it —
//!   *extending* the history rather than rebuilding it, so the session never
//!   forked.
//!
//! Two properties of the real client that the source reading did not predict,
//! and that anything asserting on this path has to know. Codex renders an MCP
//! result as `"Wall time: … seconds\nOutput:\n[…]"`, so a tool's output is
//! matched by containment and never by equality. And under
//! `approval_policy = "never"` a tool carrying no MCP annotations is treated
//! as destructive and open-world, and its call is **cancelled** — the agent
//! receiving a cancellation notice where the output should have been, with
//! nothing in the turn saying so.
//!
//! Both halves of the answer to that second one are now in the tree, and they
//! are not redundant. Every descriptor in [`tools`] states all three hints
//! (`readOnlyHint`, `destructiveHint`, `openWorldHint`), which is the narrow
//! answer and the only one that reaches a client we never handed a config to.
//! **That last half is read and not yet observed** — `requires_mcp_tool_approval`
//! (`core/src/mcp_tool_call.rs:2156-2173` @ `e363b08`) consults the hints
//! under codex's default `Auto` mode ahead of any config at all, but every
//! e2e run in this tree drives a client holding the generated config, so what
//! the binary does with an *un*configured stanza is source reading of the same
//! kind the block above replaced. The generated launch config
//! (`crates/roundhouse-server/src/codex_launch.rs`) keeps
//! `default_tools_approval_mode = "approve"` beside them, as the Direct
//! topology's defense in depth rather than as the fix: `codex exec` forces
//! `approval_policy = "never"`, so a client that disagreed with our hints for
//! any reason would cancel a writer rather than prompt about it, and there is
//! no interactive operator in that topology to prompt. Scoping the grant to
//! the reads instead — the narrower-looking option — was considered and
//! refused for exactly that reason; the ruling and its citations live beside
//! the generated stanza.
//!
//! *[History — recorded so nobody re-litigates it. Until M9 this block was a
//! documented assumption citing the then-current Cargo pin `6344a65`:
//! `codex-rs/mcp-client/src/router.rs:164` for transport selection by config
//! shape, and `codex-rs/core/src/mcp/registry.rs:440-444` for dispatch and
//! append. The facts are restated above against `e363b08` rather than
//! re-pinned to those paths, because `e363b08` is the tree the binary under
//! test was built from and the two revs are not on one line of descent —
//! neither is an ancestor of the other, and the MCP client was reorganized
//! between them. The Cargo pin itself is unchanged; M9 bumped nothing.]*
//!
//! # A third property, unread: codex names the conversation on every call
//!
//! Codex stamps `params._meta.threadId` on **every** `tools/call` it
//! dispatches — `with_mcp_tool_call_thread_id_meta`
//! (`core/src/mcp_tool_call.rs:1198-1220` @ `e363b08`, called at line 442
//! with no conditional guard) inserts `sess.thread_id`, and captured traffic
//! shows it byte-identical to the `prompt_cache_key` on the same turn's
//! `/v1/responses` bodies. It rides a `_meta` object that also carries
//! `x-codex-turn-metadata.session_id`.
//!
//! **Nothing here reads it.** `ControlPlaneReads::resolve_session` resolves
//! from the tool call's own `conversation` argument (qualified into the
//! caller's namespace) or from `Conversations::latest`, and
//! [`fetch_steer`](ControlSurface::fetch_steer) resolves from `steer_id`
//! alone; today's isolation is therefore tenant-scoped — a `Principal` plus a
//! qualified name — and not thread-id-based. Recorded because the block above
//! says what M9 proved about dispatch and resend, and a reader could take
//! that for an audit of every field the client sends. It is not:
//! `codexs_meta_thread_id_rides_every_tools_call_and_is_never_read`
//! (`crates/roundhouse-server/tests/codex_e2e.rs`) passes *today*, and its
//! passing is the point.
//!
//! Wiring it in is deferred rather than pending. `_meta.threadId` is the
//! codex-native shortcut and would bind this surface to one client's
//! conventions; [`init_session`](ControlSurface::init_session) is the
//! client-agnostic path to the same correlation and is the one the plan
//! carries. A cross-check between the two — the correlator we are handed
//! versus the token we minted — is a defense-in-depth design decision with
//! its own failure modes (what a disagreement means), not a fix for anything
//! broken.
//!
//! # Note the tense: `init_session` is still write-only
//!
//! [`init_session`](ControlSurface::init_session) mints an id, records it and
//! returns it in a form a client keeps. The id reaches a session log only by
//! riding the client's own resent history, and the session whose log holds it
//! is the session that made the call. M9 proves the *carriage* — a real client
//! does resend a tool's output verbatim into the next turn — but nothing in
//! this deployment resolves a session from a binding yet: [`binding_in_items`]
//! has no caller outside tests. The read side was noted here as M7's; M7 has
//! since landed (real frontier credentials) without it, so it belongs to no
//! rung at present rather than to that one. Both agent-facing sentences about
//! it ([`tools::descriptors`] and [`surface::InitSessionResponse::note`]) are
//! still written to say what is recorded rather than what is correlated, which
//! is what keeps the gap honest in the one place a model can read.

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
